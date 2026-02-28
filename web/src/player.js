import { reactive } from 'vue'

const HEADER_SIZE = 16
const SERVER_FPS = 60
const FRAME_TIMESTAMP_US = Math.round(1_000_000 / SERVER_FPS)
const BASE_CONTROL_HINT = 'Moonlight 快捷键: Ctrl+Alt+Shift+Z 接管/释放 · S 统计 · X 全屏 · M 显示器 · E 编码 · Q 断开/重连'

const ENCODING_DEFAULTS = Object.freeze({
  fps: 60,
  bitrateMbps: 20,
  keyframeInterval: 2,
})

const ENCODING_LIMITS = Object.freeze({
  fps: { min: 24, max: 120 },
  bitrateMbps: { min: 2, max: 80 },
  keyframeInterval: { min: 1, max: 10 },
})

const FRAME_TYPE = {
  VIDEO: 0x01,
  KEYFRAME_REQUEST: 0x02,
  MONITOR_LIST: 0x04,
  MONITOR_SELECT: 0x05,
  MOUSE_INPUT: 0x06,
  KEYBOARD_INPUT: 0x07,
  ENCODING_SETTINGS: 0x08,
}

const FRAME_FLAGS = {
  KEYFRAME: 0x01,
}

const PLAYER_GLOBAL_KEY = '__webdisplayPlayer'

export const createUiState = () =>
  reactive({
    overlayVisible: true,
    connectionVisible: true,
    connected: false,
    connectionText: '正在连接...',
    connectionDetail: '',
    monitorPickerVisible: false,
    monitors: [],
    activeMonitorIndex: null,
    encodingPanelVisible: false,
    encodingDraft: { ...ENCODING_DEFAULTS },
    controlHintVisible: true,
    controlHintText: BASE_CONTROL_HINT,
    showLocalCursor: false,
    stats: {
      latency: '--',
      fps: '--',
      fpsClass: '',
      decode: '--',
      queue: '--',
      bitrate: '--',
    },
  })

export class UltraLowLatencyPlayer {
  constructor(canvas, uiState) {
    const context = canvas.getContext('2d')
    if (!context) {
      throw new Error('无法获取 2D 渲染上下文')
    }

    this.canvas = canvas
    this.ctx = context
    this.ui = uiState

    this.decoder = null
    this.ws = null
    this.connected = false
    this.autoReconnect = true
    this.reconnectTimer = null
    this.hintTimer = null
    this.hintHideTimer = null
    this.statsTimer = null

    this.stats = {
      framesDecoded: 0,
      framesDropped: 0,
      lastFpsTime: performance.now(),
      fpsCount: 0,
      fps: 0,
      decodeTimeMs: 0,
      bitrateBytes: 0,
      bitrateMbps: 0,
    }

    this.controlActive = false
    this.pressedKeys = new Map()
    this.pressedButtons = new Set()
    this.lastPointerPos = { x: 0.5, y: 0.5 }
    this.pendingMouseMoveEvent = null
    this.mouseMoveScheduled = false
    this.showLocalCursor = false
    this.lockPointerToVideo = false

    this.textEncoder = new TextEncoder()
    this.textDecoder = new TextDecoder()
    this.canvas.tabIndex = 0

    this.encodingSettings = { ...ENCODING_DEFAULTS }
    this.ui.encodingDraft = { ...this.encodingSettings }
    this.ui.controlHintText = BASE_CONTROL_HINT

    this.pendingFrame = null
    this.renderBound = this._renderLoop.bind(this)

    const scopedWindow = window
    const previousPlayer = scopedWindow[PLAYER_GLOBAL_KEY]
    if (previousPlayer && previousPlayer !== this && typeof previousPlayer.destroy === 'function') {
      previousPlayer.destroy()
    }
    scopedWindow[PLAYER_GLOBAL_KEY] = this

    this._init()
  }

  destroy() {
    this.autoReconnect = false

    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }

    if (this.hintTimer) {
      clearTimeout(this.hintTimer)
      this.hintTimer = null
    }

    if (this.hintHideTimer) {
      clearTimeout(this.hintHideTimer)
      this.hintHideTimer = null
    }

    if (this.statsTimer) {
      clearInterval(this.statsTimer)
      this.statsTimer = null
    }

    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      this.ws.close(1000, 'player destroy')
    }
    this.ws = null

    if (this.pendingFrame) {
      this.pendingFrame.close()
      this.pendingFrame = null
    }

    if (this.decoder && this.decoder.state !== 'closed') {
      try {
        this.decoder.close()
      } catch (_) {
      }
    }
    this.decoder = null

    this._deactivateControl()

    const scopedWindow = window
    if (scopedWindow[PLAYER_GLOBAL_KEY] === this) {
      delete scopedWindow[PLAYER_GLOBAL_KEY]
    }
  }

  _setConnectionState({ visible = true, connected = false, text = '', detail = '' }) {
    this.ui.connectionVisible = visible
    this.ui.connected = connected
    this.ui.connectionText = text
    this.ui.connectionDetail = detail
  }

  async _init() {
    if (typeof VideoDecoder === 'undefined') {
      this._setConnectionState({
        visible: true,
        connected: false,
        text: '❌ 浏览器不支持 WebCodecs API',
        detail: '请使用 Chrome 94+ 或 Edge 94+',
      })
      return
    }

    const support = await VideoDecoder.isConfigSupported({
      codec: 'av01.0.08M.08',
      hardwareAcceleration: 'prefer-hardware',
    })

    if (!support.supported) {
      this._setConnectionState({
        visible: true,
        connected: false,
        text: '❌ 浏览器不支持 AV1 硬件解码',
        detail: '',
      })
      return
    }

    this._initDecoder()
    this._connect()
    requestAnimationFrame(this.renderBound)
    this._startStatsUpdate()
    this._bindInputEvents()
    this._showControlHint(4500)
  }

  _initDecoder() {
    this.decoder = new VideoDecoder({
      output: (frame) => {
        const oldFrame = this.pendingFrame
        this.pendingFrame = frame
        if (oldFrame) {
          oldFrame.close()
          this.stats.framesDropped++
        }
        this.stats.framesDecoded++
      },
      error: (error) => {
        console.error('解码错误:', error)
        this._resetDecoder()
        this._requestKeyframe()
      },
    })

    this.decoder.configure({
      codec: 'av01.0.08M.08',
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    })

    console.log('AV1 解码器已初始化 (硬件加速, 低延迟模式)')
  }

  _connect() {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }

    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      return
    }

    const wsProtocol = location.protocol === 'https:' ? 'wss' : 'ws'
    const wsUrl = `${wsProtocol}://${location.host}/ws`
    console.log('连接到:', wsUrl)

    this._setConnectionState({
      visible: true,
      connected: false,
      text: '正在连接...',
      detail: '',
    })

    this.ws = new WebSocket(wsUrl)
    this.ws.binaryType = 'arraybuffer'

    this.ws.onopen = () => {
      console.log('WebSocket 已连接')
      this.connected = true
      this.autoReconnect = true
      this._setConnectionState({
        visible: false,
        connected: true,
        text: '已连接',
        detail: '',
      })
      this._syncEncodingSettings(false)
      this._requestKeyframe()
    }

    this.ws.onmessage = (event) => {
      if (!(event.data instanceof ArrayBuffer)) {
        return
      }

      this._handleMessage(event.data)
    }

    this.ws.onclose = () => {
      console.log('WebSocket 已断开')
      this.connected = false
      this._releaseAllInputs()
      this.controlActive = false
      this._exitPointerLock()
      this._applyCursorVisibility()

      if (this.autoReconnect) {
        this._setConnectionState({
          visible: true,
          connected: false,
          text: '连接断开，3秒后重连...',
          detail: '',
        })
        this.reconnectTimer = setTimeout(() => {
          this.reconnectTimer = null
          this._connect()
        }, 3000)
      } else {
        this._setConnectionState({
          visible: true,
          connected: false,
          text: '会话已退出，按 Ctrl+Alt+Shift+Q 重连',
          detail: '',
        })
      }
    }

    this.ws.onerror = (error) => {
      console.error('WebSocket 错误:', error)
    }
  }

  _clampInt(rawValue, min, max, fallback) {
    const value = Number.parseInt(rawValue, 10)
    if (!Number.isFinite(value)) {
      return fallback
    }
    return Math.min(Math.max(value, min), max)
  }

  _normalizeEncodingDraft(draft, fallback = ENCODING_DEFAULTS) {
    const source = draft && typeof draft === 'object' ? draft : {}

    return {
      bitrateMbps: this._clampInt(
        source.bitrateMbps,
        ENCODING_LIMITS.bitrateMbps.min,
        ENCODING_LIMITS.bitrateMbps.max,
        fallback.bitrateMbps,
      ),
      fps: this._clampInt(source.fps, ENCODING_LIMITS.fps.min, ENCODING_LIMITS.fps.max, fallback.fps),
      keyframeInterval: this._clampInt(
        source.keyframeInterval,
        ENCODING_LIMITS.keyframeInterval.min,
        ENCODING_LIMITS.keyframeInterval.max,
        fallback.keyframeInterval,
      ),
    }
  }

  applyEncodingSettings() {
    this._applyEncodingSettings()
  }

  resetEncodingSettings() {
    this.ui.encodingDraft = { ...ENCODING_DEFAULTS }
    this._applyEncodingSettings()
  }

  selectMonitor(index) {
    this._requestMonitorSwitch(index)
  }

  _syncEncodingSettings(requestKeyframe = true) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return false
    }

    this._sendJsonControlPacket(FRAME_TYPE.ENCODING_SETTINGS, {
      fps: this.encodingSettings.fps,
      bitrate: this.encodingSettings.bitrateMbps * 1_000_000,
      keyframe_interval: this.encodingSettings.keyframeInterval,
    })

    if (requestKeyframe) {
      this._requestKeyframe()
    }

    return true
  }

  _applyEncodingSettings() {
    const normalized = this._normalizeEncodingDraft(this.ui.encodingDraft, this.encodingSettings)
    this.encodingSettings = { ...normalized }
    this.ui.encodingDraft = { ...normalized }

    const syncOk = this._syncEncodingSettings(true)
    if (syncOk) {
      this._flashHint(`编码设置已应用: ${this.encodingSettings.fps}fps / ${this.encodingSettings.bitrateMbps}Mbps`)
    } else {
      this._flashHint('编码设置已保存，将在重连后自动应用')
    }
  }

  _toggleEncodingPanel() {
    const willOpen = !this.ui.encodingPanelVisible
    if (willOpen) {
      this.ui.encodingDraft = { ...this.encodingSettings }
      this.ui.monitorPickerVisible = false
    }
    this.ui.encodingPanelVisible = willOpen
  }

  _bindInputEvents() {
    window.addEventListener(
      'keydown',
      (event) => {
        if (this._handleMoonlightShortcutKeyDown(event)) {
          return
        }

        if (!this.controlActive) {
          return
        }

        if (event.key === 'Escape') {
          event.preventDefault()
          this._deactivateControl()
          return
        }

        if (event.isComposing) {
          return
        }

        event.preventDefault()
        this._sendKeyboardInput(event, true)
      },
      true,
    )

    window.addEventListener(
      'keyup',
      (event) => {
        if (this._handleMoonlightShortcutKeyUp(event)) {
          return
        }

        if (!this.controlActive || event.key === 'Escape' || event.isComposing) {
          return
        }

        event.preventDefault()
        this._sendKeyboardInput(event, false)
      },
      true,
    )

    this.canvas.addEventListener('mousedown', (event) => {
      event.preventDefault()
      this._activateControl()

      const pos = this._capturePointerPositionFromMouse(event)
      this.pressedButtons.add(event.button)
      this._sendMouseInput({
        kind: 'button',
        x: pos.x,
        y: pos.y,
        button: event.button,
        down: true,
      })
    })

    this.canvas.addEventListener('mousemove', (event) => {
      if (!this.controlActive) {
        return
      }

      if (this._isPointerLocked()) {
        this.pendingMouseMoveEvent = {
          mode: 'relative',
          movementX: event.movementX,
          movementY: event.movementY,
        }
      } else {
        this.pendingMouseMoveEvent = {
          mode: 'absolute',
          clientX: event.clientX,
          clientY: event.clientY,
        }
      }

      this._scheduleMouseMove()
    })

    const releaseMouseButton = (event) => {
      if (!this.controlActive || !this.pressedButtons.has(event.button)) {
        return
      }

      event.preventDefault()
      const pos = this._capturePointerPositionFromMouse(event)
      this.pressedButtons.delete(event.button)
      this._sendMouseInput({
        kind: 'button',
        x: pos.x,
        y: pos.y,
        button: event.button,
        down: false,
      })
    }

    this.canvas.addEventListener('mouseup', releaseMouseButton)
    window.addEventListener('mouseup', releaseMouseButton, true)

    this.canvas.addEventListener(
      'wheel',
      (event) => {
        if (!this.controlActive) {
          return
        }

        event.preventDefault()
        const pos = this._capturePointerPositionFromMouse(event)
        const unit =
          event.deltaMode === WheelEvent.DOM_DELTA_LINE
            ? 40
            : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
              ? 120
              : 1

        const deltaX = Math.round(event.deltaX * unit)
        const deltaY = Math.round(-event.deltaY * unit)
        if (deltaX === 0 && deltaY === 0) {
          return
        }

        this._sendMouseInput({
          kind: 'wheel',
          x: pos.x,
          y: pos.y,
          delta_x: deltaX,
          delta_y: deltaY,
        })
      },
      { passive: false },
    )

    this.canvas.addEventListener('contextmenu', (event) => {
      if (this.controlActive) {
        event.preventDefault()
      }
    })

    window.addEventListener('blur', () => {
      this._deactivateControl()
    })

    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        this._deactivateControl()
      }
    })

    document.addEventListener('pointerlockchange', () => {
      if (this.lockPointerToVideo && this.controlActive && !this._isPointerLocked()) {
        this._flashHint('鼠标锁定已解除，按 Ctrl+Alt+Shift+L 重新锁定')
      }
    })

    window.addEventListener(
      'mousemove',
      (event) => {
        if (event.clientY >= window.innerHeight - 72) {
          this._showControlHint(2400)
        }
      },
      { passive: true },
    )

    window.addEventListener(
      'touchstart',
      () => {
        this._showControlHint(2400)
      },
      { passive: true },
    )
  }

  _isMoonlightShortcutChord(event) {
    return event.ctrlKey && event.altKey && event.shiftKey && !event.metaKey
  }

  _moonlightShortcutAction(code) {
    switch (code) {
      case 'KeyQ':
        return 'quit'
      case 'KeyZ':
        return 'capture'
      case 'KeyX':
        return 'fullscreen'
      case 'KeyS':
        return 'stats'
      case 'KeyM':
        return 'monitor'
      case 'KeyE':
        return 'encoding'
      case 'KeyL':
        return 'lock'
      case 'KeyC':
        return 'cursor'
      case 'KeyV':
        return 'clipboard'
      case 'KeyD':
        return 'minimize'
      default:
        return null
    }
  }

  _handleMoonlightShortcutKeyDown(event) {
    if (!this._isMoonlightShortcutChord(event)) {
      return false
    }

    const action = this._moonlightShortcutAction(event.code)
    if (!action) {
      return false
    }

    event.preventDefault()
    event.stopPropagation()
    this._showControlHint(2600)

    if (event.repeat) {
      return true
    }

    switch (action) {
      case 'quit':
        this._toggleStreamSession()
        break
      case 'capture':
        if (this.controlActive) {
          this._deactivateControl()
          this._flashHint('输入控制已释放')
        } else {
          this._activateControl()
          this._flashHint('输入控制已激活')
        }
        break
      case 'fullscreen':
        this._toggleFullscreen()
        break
      case 'stats':
        this.ui.overlayVisible = !this.ui.overlayVisible
        break
      case 'monitor':
        this.ui.encodingPanelVisible = false
        this.ui.monitorPickerVisible = !this.ui.monitorPickerVisible
        break
      case 'encoding':
        this._toggleEncodingPanel()
        break
      case 'lock':
        this.lockPointerToVideo = !this.lockPointerToVideo
        if (!this.controlActive || this.showLocalCursor) {
          if (!this.lockPointerToVideo) {
            this._exitPointerLock()
          }
        } else if (this.lockPointerToVideo) {
          this._requestPointerLock()
        } else {
          this._exitPointerLock()
        }
        this._flashHint(`鼠标锁定${this.lockPointerToVideo ? '已开启' : '已关闭'}`)
        break
      case 'cursor':
        this.showLocalCursor = !this.showLocalCursor
        this._applyCursorVisibility()
        if (this.showLocalCursor) {
          this._exitPointerLock()
        } else if (this.controlActive && this.lockPointerToVideo) {
          this._requestPointerLock()
        }
        this._flashHint(`本地光标${this.showLocalCursor ? '显示' : '隐藏'}`)
        break
      case 'clipboard':
        this._flashHint('浏览器版暂不支持 Ctrl+Alt+Shift+V 粘贴')
        break
      case 'minimize':
        this._flashHint('浏览器版暂不支持 Ctrl+Alt+Shift+D 最小化')
        break
      default:
        break
    }

    return true
  }

  _handleMoonlightShortcutKeyUp(event) {
    if (!this._isMoonlightShortcutChord(event)) {
      return false
    }

    const action = this._moonlightShortcutAction(event.code)
    if (!action) {
      return false
    }

    event.preventDefault()
    event.stopPropagation()
    return true
  }

  _toggleStreamSession() {
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      this.autoReconnect = false
      this._deactivateControl()
      this.ws.close(1000, 'client quit')
      this._flashHint('会话已退出')
      return
    }

    this.autoReconnect = true
    this._setConnectionState({
      visible: true,
      connected: false,
      text: '正在连接...',
      detail: '',
    })
    this._connect()
    this._flashHint('正在重新连接')
  }

  _toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {})
    } else {
      document.documentElement.requestFullscreen().catch(() => {})
    }
  }

  _showControlHint(autoHideMs = 3000) {
    this.ui.controlHintVisible = true

    if (this.hintHideTimer) {
      clearTimeout(this.hintHideTimer)
      this.hintHideTimer = null
    }

    if (autoHideMs <= 0) {
      return
    }

    this.hintHideTimer = setTimeout(() => {
      this.ui.controlHintVisible = false
      this.hintHideTimer = null
    }, autoHideMs)
  }

  _flashHint(text) {
    this.ui.controlHintText = text
    this._showControlHint(2500)

    if (this.hintTimer) {
      clearTimeout(this.hintTimer)
    }

    this.hintTimer = setTimeout(() => {
      this.ui.controlHintText = BASE_CONTROL_HINT
      this._showControlHint(3200)
      this.hintTimer = null
    }, 2200)
  }

  _isPointerLocked() {
    return document.pointerLockElement === this.canvas
  }

  _requestPointerLock() {
    if (this._isPointerLocked()) {
      return
    }

    this.canvas.requestPointerLock()
  }

  _exitPointerLock() {
    if (this._isPointerLocked()) {
      document.exitPointerLock()
    }
  }

  _applyCursorVisibility() {
    this.ui.showLocalCursor = this.showLocalCursor
  }

  _activateControl() {
    if (this.controlActive) {
      return
    }

    this.controlActive = true
    this.canvas.focus({ preventScroll: true })
    this._applyCursorVisibility()

    if (this.lockPointerToVideo && !this.showLocalCursor) {
      this._requestPointerLock()
    }
  }

  _deactivateControl() {
    if (!this.controlActive && this.pressedKeys.size === 0 && this.pressedButtons.size === 0) {
      return
    }

    this._releaseAllInputs()
    this.controlActive = false
    this.pendingMouseMoveEvent = null
    this.mouseMoveScheduled = false
    this._exitPointerLock()
    this._applyCursorVisibility()
    this.canvas.blur()
  }

  _releaseAllInputs() {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      for (const keyInfo of this.pressedKeys.values()) {
        this._sendJsonControlPacket(FRAME_TYPE.KEYBOARD_INPUT, {
          key_code: keyInfo.keyCode,
          code: keyInfo.code,
          down: false,
        })
      }

      for (const button of this.pressedButtons.values()) {
        this._sendMouseInput({
          kind: 'button',
          x: this.lastPointerPos.x,
          y: this.lastPointerPos.y,
          button,
          down: false,
        })
      }
    }

    this.pressedKeys.clear()
    this.pressedButtons.clear()
  }

  _scheduleMouseMove() {
    if (this.mouseMoveScheduled) {
      return
    }

    this.mouseMoveScheduled = true
    requestAnimationFrame(() => {
      this.mouseMoveScheduled = false

      if (!this.controlActive || !this.pendingMouseMoveEvent) {
        return
      }

      let pos = this.lastPointerPos
      if (this.pendingMouseMoveEvent.mode === 'relative') {
        pos = this._normalizePointerDelta(
          this.pendingMouseMoveEvent.movementX,
          this.pendingMouseMoveEvent.movementY,
        )
      } else {
        pos = this._normalizePointer(
          this.pendingMouseMoveEvent.clientX,
          this.pendingMouseMoveEvent.clientY,
        )
      }

      this.pendingMouseMoveEvent = null
      this.lastPointerPos = pos
      this._sendMouseInput({ kind: 'move', x: pos.x, y: pos.y })
    })
  }

  _capturePointerPositionFromMouse(event) {
    const pos = this._normalizePointer(event.clientX, event.clientY)
    this.lastPointerPos = pos
    return pos
  }

  _normalizePointer(clientX, clientY) {
    if (typeof clientX !== 'number' || typeof clientY !== 'number') {
      return this.lastPointerPos
    }

    const rect = this.canvas.getBoundingClientRect()
    if (rect.width <= 0 || rect.height <= 0) {
      return this.lastPointerPos
    }

    const x = Math.min(Math.max((clientX - rect.left) / rect.width, 0), 1)
    const y = Math.min(Math.max((clientY - rect.top) / rect.height, 0), 1)
    return { x, y }
  }

  _normalizePointerDelta(movementX, movementY) {
    const rect = this.canvas.getBoundingClientRect()
    if (rect.width <= 0 || rect.height <= 0) {
      return this.lastPointerPos
    }

    const x = Math.min(Math.max(this.lastPointerPos.x + movementX / rect.width, 0), 1)
    const y = Math.min(Math.max(this.lastPointerPos.y + movementY / rect.height, 0), 1)
    return { x, y }
  }

  _sendMouseInput(payload) {
    this._sendJsonControlPacket(FRAME_TYPE.MOUSE_INPUT, payload)
  }

  _sendKeyboardInput(event, down) {
    const keyCode = event.keyCode || event.which || 0
    const code = event.code || null
    if (keyCode === 0 && !code) {
      return
    }

    const keyId = code || `kc-${keyCode}`
    if (down) {
      this.pressedKeys.set(keyId, { keyCode, code })
    } else {
      this.pressedKeys.delete(keyId)
    }

    this._sendJsonControlPacket(FRAME_TYPE.KEYBOARD_INPUT, {
      key_code: keyCode,
      code,
      down,
    })
  }

  _sendJsonControlPacket(frameType, payloadObj) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return
    }

    const payload = this.textEncoder.encode(JSON.stringify(payloadObj))
    const header = new ArrayBuffer(HEADER_SIZE)
    const view = new DataView(header)
    view.setUint8(0, frameType)
    view.setUint32(10, payload.byteLength, true)

    const packet = new Uint8Array(HEADER_SIZE + payload.byteLength)
    packet.set(new Uint8Array(header), 0)
    packet.set(payload, HEADER_SIZE)
    this.ws.send(packet.buffer)
  }

  _handleMessage(data) {
    if (data.byteLength < HEADER_SIZE) {
      return
    }

    const view = new DataView(data)
    const frameType = view.getUint8(0)
    const flags = view.getUint8(1)
    const sequence = view.getUint32(2, true)
    const payloadLen = view.getUint32(10, true)

    if (HEADER_SIZE + payloadLen > data.byteLength) {
      return
    }

    const payload = new Uint8Array(data, HEADER_SIZE, payloadLen)

    if (frameType === FRAME_TYPE.MONITOR_LIST) {
      try {
        const jsonStr = this.textDecoder.decode(payload)
        const monitors = JSON.parse(jsonStr)
        this._updateMonitorList(Array.isArray(monitors) ? monitors : [])
      } catch (error) {
        console.error('解析显示器列表失败', error)
      }
      return
    }

    if (frameType !== FRAME_TYPE.VIDEO) {
      return
    }

    const isKeyframe = (flags & FRAME_FLAGS.KEYFRAME) !== 0
    this.stats.bitrateBytes += data.byteLength

    const decodeStart = performance.now()

    try {
      if (!this.decoder || this.decoder.state !== 'configured') {
        return
      }

      const queueSize = this.decoder.decodeQueueSize
      if (queueSize > 10) {
        console.warn(`解码队列过长(${queueSize}), 重置`)
        this._resetDecoder()
        this._requestKeyframe()
        return
      }

      const chunk = new EncodedVideoChunk({
        type: isKeyframe ? 'key' : 'delta',
        timestamp: sequence * FRAME_TIMESTAMP_US,
        data: payload,
      })

      this.decoder.decode(chunk)
      this.stats.decodeTimeMs = performance.now() - decodeStart
    } catch (error) {
      console.error('送入解码器失败:', error)
      this._resetDecoder()
      this._requestKeyframe()
    }
  }

  _renderLoop() {
    const frame = this.pendingFrame
    if (frame) {
      this.pendingFrame = null

      if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
        this.canvas.width = frame.displayWidth
        this.canvas.height = frame.displayHeight
      }

      this.ctx.drawImage(frame, 0, 0)
      frame.close()
      this.stats.fpsCount++
    }

    requestAnimationFrame(this.renderBound)
  }

  _resetDecoder() {
    if (this.decoder && this.decoder.state !== 'closed') {
      try {
        this.decoder.close()
      } catch (_) {
      }
    }

    this.decoder = null
    this._initDecoder()
  }

  _requestKeyframe() {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return
    }

    const header = new ArrayBuffer(HEADER_SIZE)
    const view = new DataView(header)
    view.setUint8(0, FRAME_TYPE.KEYFRAME_REQUEST)
    this.ws.send(header)
  }

  _updateMonitorList(monitors) {
    this.ui.monitors = monitors

    if (monitors.length === 0) {
      this.ui.activeMonitorIndex = null
      return
    }

    const hasCurrent = monitors.some((item) => item.index === this.ui.activeMonitorIndex)
    if (!hasCurrent) {
      const primary = monitors.find((item) => item.primary)
      this.ui.activeMonitorIndex = primary ? primary.index : monitors[0].index
    }
  }

  _requestMonitorSwitch(index) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return
    }

    this._sendJsonControlPacket(FRAME_TYPE.MONITOR_SELECT, { index })
    this.ui.activeMonitorIndex = index
    this.ui.monitorPickerVisible = false
    this._requestKeyframe()
  }

  _startStatsUpdate() {
    this.statsTimer = setInterval(() => {
      const now = performance.now()
      const elapsed = (now - this.stats.lastFpsTime) / 1000

      if (elapsed >= 1) {
        this.stats.fps = Math.round(this.stats.fpsCount / elapsed)
        this.stats.bitrateMbps = (this.stats.bitrateBytes * 8) / elapsed / 1_000_000
        this.stats.fpsCount = 0
        this.stats.bitrateBytes = 0
        this.stats.lastFpsTime = now
      }

      this.ui.stats.latency = `${this.stats.decodeTimeMs.toFixed(1)}ms`
      this.ui.stats.fps = `${this.stats.fps}`
      this.ui.stats.decode = `${this.stats.decodeTimeMs.toFixed(2)}ms`
      this.ui.stats.queue = this.decoder ? `${this.decoder.decodeQueueSize}` : '--'
      this.ui.stats.bitrate = `${this.stats.bitrateMbps.toFixed(1)} Mbps`
      this.ui.stats.fpsClass = this.stats.fps < 30 ? 'bad' : this.stats.fps < 55 ? 'warn' : ''
    }, 200)
  }
}

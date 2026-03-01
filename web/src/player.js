import { reactive } from 'vue'

const HEADER_SIZE = 16
const BASE_CONTROL_HINT = 'Moonlight 快捷键: Ctrl+Alt+Shift+Z 接管/释放 · S 统计 · X 全屏 · M 显示器 · E 编码 · Q 断开/重连'
const RECONNECT_DELAY_MS = 3000
const WEBTRANSPORT_PATH = '/webtransport'
const WEBTRANSPORT_HASH_PATH = '/webtransport/hash'
const MAX_WEBTRANSPORT_PACKET_SIZE = 64 * 1024 * 1024

const CODEC_PRESETS = Object.freeze([
  {
    id: 'avc',
    label: 'AVC',
    decoderCandidates: ['avc1.640028', 'avc1.4D401F', 'avc1.42E01E'],
  },
  {
    id: 'hevc',
    label: 'HEVC',
    decoderCandidates: ['hvc1.1.6.L93.B0', 'hev1.1.6.L93.B0'],
  },
  {
    id: 'av1',
    label: 'AV1',
    decoderCandidates: ['av01.0.08M.08'],
  },
])

const CODEC_ID_ALIASES = Object.freeze({
  h264: 'avc',
  h265: 'hevc',
})

const ENCODING_DEFAULTS = Object.freeze({
  codec: 'avc',
  fps: 60,
  bitrateMbps: 20,
  keyframeInterval: 2,
})

const HIGH_FPS_HINT_THRESHOLD = 72

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

const WEBRTC_OFFER_PATH = '/webrtc/offer'

const PLAYER_GLOBAL_KEY = '__webdisplayPlayer'

export const createUiState = () =>
  reactive({
    availableCodecs: [],
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
    const context =
      canvas.getContext('2d', { alpha: false, desynchronized: true }) || canvas.getContext('2d')
    if (!context) {
      throw new Error('无法获取 2D 渲染上下文')
    }

    this.canvas = canvas
    this.ctx = context
    this.ui = uiState

    this.decoder = null
    this.ws = null
    this.webTransport = null
    this.webTransportWriter = null
    this.webTransportReader = null
    this.webTransportReadBuffer = new Uint8Array(0)

    this.webrtc = null
    this.webrtcDataChannel = null

    this.transportKind = null
    this.transportCloseHandled = false
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

    this.supportedCodecConfigs = new Map()
    this.activeDecoderCodecId = null

    this.encodingSettings = { ...ENCODING_DEFAULTS }
    this.ui.encodingDraft = { ...this.encodingSettings }
    this.ui.controlHintText = BASE_CONTROL_HINT

    this.pendingFrame = null
    this.renderBound = this._renderLoop.bind(this)
    this.lastChunkTimestampUs = 0

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

    this._closeTransport('player destroy')
    this._resetTransportState()

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
    this.activeDecoderCodecId = null

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

  _resetTransportState() {
    this.ws = null
    this.webTransport = null
    this.webTransportWriter = null
    this.webTransportReader = null
    this.webTransportReadBuffer = new Uint8Array(0)

    this.webrtc = null
    this.webrtcDataChannel = null

    this.transportKind = null
  }

  _isTransportOpen() {
    if (this.transportKind === 'websocket') {
      return this.ws && this.ws.readyState === WebSocket.OPEN
    }

    if (this.transportKind === 'webtransport') {
      return !!(this.webTransport && this.webTransportWriter && this.webTransportReader)
    }

    if (this.transportKind === 'webrtc') {
      return this.webrtcDataChannel && this.webrtcDataChannel.readyState === 'open'
    }

    return false
  }

  _isTransportActive() {
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      return true
    }

    if (this.webrtc && (this.webrtc.connectionState !== 'closed' && this.webrtc.connectionState !== 'failed')) {
      return true
    }

    return !!this.webTransport
  }

  _closeTransport(reason = 'client close') {
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      this.ws.close(1000, reason)
    }

    if (this.webTransportReader) {
      this.webTransportReader.cancel(reason).catch(() => { })
      try {
        this.webTransportReader.releaseLock()
      } catch (_) {
      }
    }

    if (this.webTransportWriter) {
      this.webTransportWriter.close().catch(() => { })
      try {
        this.webTransportWriter.releaseLock()
      } catch (_) {
      }
    }

    if (this.webTransport) {
      try {
        this.webTransport.close({ closeCode: 0, reason })
      } catch (_) {
        try {
          this.webTransport.close()
        } catch (_) {
        }
      }
    }

    if (this.webrtcDataChannel) {
      try { this.webrtcDataChannel.close() } catch (_) { }
    }
    if (this.webrtc) {
      try { this.webrtc.close() } catch (_) { }
    }
  }

  _onTransportDisconnected(reasonText = '连接已断开') {
    if (this.transportCloseHandled) {
      return
    }

    this.transportCloseHandled = true
    this._resetTransportState()
    this.connected = false
    this._releaseAllInputs()
    this.controlActive = false
    this._exitPointerLock()
    this._applyCursorVisibility()

    if (this.autoReconnect) {
      this._setConnectionState({
        visible: true,
        connected: false,
        text: `${reasonText}，3秒后重连...`,
        detail: '',
      })
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null
        this.transportCloseHandled = false
        void this._connect()
      }, RECONNECT_DELAY_MS)
    } else {
      this._setConnectionState({
        visible: true,
        connected: false,
        text: '会话已退出，按 Ctrl+Alt+Shift+Q 重连',
        detail: '',
      })
    }
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

    const supportedCodecs = await this._detectSupportedCodecs()
    if (supportedCodecs.length === 0) {
      this._setConnectionState({
        visible: true,
        connected: false,
        text: '❌ 浏览器不支持 AV1 / HEVC / AVC 硬件解码',
        detail: '',
      })
      return
    }

    this.supportedCodecConfigs.clear()
    for (const item of supportedCodecs) {
      this.supportedCodecConfigs.set(item.id, item)
    }

    this.ui.availableCodecs = supportedCodecs.map((item) => ({
      id: item.id,
      label: item.label,
    }))

    const initialCodec = this._pickInitialCodec(supportedCodecs)
    this.encodingSettings = {
      ...ENCODING_DEFAULTS,
      codec: initialCodec,
    }
    this.ui.encodingDraft = { ...this.encodingSettings }

    this._initDecoder(initialCodec)
    void this._connect()
    requestAnimationFrame(this.renderBound)
    this._startStatsUpdate()
    this._bindInputEvents()
    this._showControlHint(4500)
  }

  async _detectSupportedCodecs() {
    const supported = []

    for (const preset of CODEC_PRESETS) {
      const decoderCodec = await this._detectDecoderCodec(preset.decoderCandidates)
      if (!decoderCodec) {
        continue
      }

      supported.push({
        id: preset.id,
        label: preset.label,
        decoderCodec,
      })
    }

    return supported
  }

  async _detectDecoderCodec(candidates) {
    for (const codec of candidates) {
      try {
        const support = await VideoDecoder.isConfigSupported({
          codec,
          hardwareAcceleration: 'prefer-hardware',
          optimizeForLatency: true,
        })
        if (support.supported) {
          return codec
        }
      } catch (_) {
      }
    }

    return null
  }

  _codecLabel(codecId) {
    const config = this.supportedCodecConfigs.get(codecId)
    if (config) {
      return config.label
    }

    const preset = CODEC_PRESETS.find((item) => item.id === codecId)
    return preset ? preset.label : codecId
  }

  _pickInitialCodec(supportedCodecs) {
    const speedPriority = ['avc', 'hevc', 'av1']
    for (const codecId of speedPriority) {
      if (supportedCodecs.some((item) => item.id === codecId)) {
        return codecId
      }
    }

    return supportedCodecs[0].id
  }

  _resolveDecoderCodec(codecId) {
    const config = this.supportedCodecConfigs.get(codecId)
    return config ? config.decoderCodec : null
  }

  _initDecoder(codecId = this.encodingSettings.codec) {
    const decoderCodec = this._resolveDecoderCodec(codecId)
    if (!decoderCodec) {
      throw new Error(`未找到可用解码配置: ${codecId}`)
    }

    if (this.pendingFrame) {
      this.pendingFrame.close()
      this.pendingFrame = null
    }

    this.decoder = new VideoDecoder({
      output: (frame) => {
        const oldFrame = this.pendingFrame
        this.pendingFrame = frame
        if (oldFrame) {
          oldFrame.close()
          this.stats.framesDropped++
        }
        this.stats.framesDecoded++
        this.stats.fpsCount++
      },
      error: (error) => {
        console.error('解码错误:', error)
        this._resetDecoder()
        this._requestKeyframe()
      },
    })

    this.decoder.configure({
      codec: decoderCodec,
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    })

    this.activeDecoderCodecId = codecId
    this.awaitingKeyframe = true
    console.log(`${this._codecLabel(codecId)} 解码器已初始化 (硬件加速, 低延迟模式)`)
  }

  _switchDecoder(codecId) {
    if (codecId === this.activeDecoderCodecId && this.decoder && this.decoder.state === 'configured') {
      return
    }

    if (this.decoder && this.decoder.state !== 'closed') {
      try {
        this.decoder.close()
      } catch (_) {
      }
    }
    this.decoder = null
    this._initDecoder(codecId)
  }

  async _connect() {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }

    if (this._isTransportActive()) {
      return
    }

    this.transportCloseHandled = false
    this._setConnectionState({
      visible: true,
      connected: false,
      text: '正在连接...',
      detail: '',
    })

    const canUseWebRtc = typeof RTCPeerConnection !== 'undefined'
    if (canUseWebRtc) {
      const webrtcConnected = await this._connectWebRTC()
      if (webrtcConnected) {
        return
      }

      this._setConnectionState({
        visible: true,
        connected: false,
        text: 'WebRTC 不可用，回退 WebTransport...',
        detail: '',
      })
    }

    const canUseWebTransport = typeof WebTransport !== 'undefined' && location.protocol === 'https:'
    if (canUseWebTransport) {
      const webTransportConnected = await this._connectWebTransport()
      if (webTransportConnected) {
        return
      }

      this._setConnectionState({
        visible: true,
        connected: false,
        text: 'WebTransport 不可用，回退 WebSocket...',
        detail: '',
      })
    }

    this._connectWebSocket()
  }

  async _connectWebTransport() {
    const wtUrl = `https://${location.host}${WEBTRANSPORT_PATH}`
    console.log('尝试 WebTransport 连接:', wtUrl)

    const withTimeout = (promise, timeoutMs, timeoutMsg) =>
      Promise.race([
        promise,
        new Promise((_, reject) => {
          setTimeout(() => reject(new Error(timeoutMsg)), timeoutMs)
        }),
      ])

    let transport = null
    try {
      const certHash = await this._fetchWebTransportServerCertificateHash()
      const transportOptions = {
        allowPooling: false,
      }

      if (certHash) {
        transportOptions.serverCertificateHashes = [certHash]
      }

      transport = new WebTransport(wtUrl, transportOptions)
      this.webTransport = transport
      this.transportKind = 'webtransport'

      await withTimeout(transport.ready, 1500, 'WebTransport 握手超时')
      const bidiStream = await withTimeout(
        transport.createBidirectionalStream(),
        1500,
        'WebTransport 双向流建立超时',
      )
      this.webTransportWriter = bidiStream.writable.getWriter()
      this.webTransportReader = bidiStream.readable.getReader()
      this.webTransportReadBuffer = new Uint8Array(0)

      transport.closed
        .then(() => {
          if (this.transportKind === 'webtransport') {
            console.log('WebTransport 已断开')
            this._onTransportDisconnected('WebTransport 连接断开')
          }
        })
        .catch((error) => {
          if (this.transportKind === 'webtransport') {
            console.warn('WebTransport closed:', error)
            this._onTransportDisconnected('WebTransport 连接断开')
          }
        })

      console.log('WebTransport 已连接')
      this.connected = true
      this.autoReconnect = true
      this._setConnectionState({
        visible: false,
        connected: true,
        text: '已连接',
        detail: '传输: WebTransport',
      })
      this._syncEncodingSettings(false)
      this._requestKeyframe()
      void this._webTransportReadLoop()
      return true
    } catch (error) {
      console.warn('WebTransport 连接失败:', error)

      if (this.webTransportReader) {
        this.webTransportReader.cancel('fallback to websocket').catch(() => { })
        try {
          this.webTransportReader.releaseLock()
        } catch (_) {
        }
      }
      if (this.webTransportWriter) {
        this.webTransportWriter.close().catch(() => { })
        try {
          this.webTransportWriter.releaseLock()
        } catch (_) {
        }
      }
      if (transport) {
        try {
          transport.close()
        } catch (_) {
        }
      }

      this._resetTransportState()
      return false
    }
  }

  async _connectWebRTC() {
    console.log('尝试 WebRTC 连接...')
    try {
      this.webrtc = new RTCPeerConnection({
        iceServers: []
      })
      this.transportKind = 'webrtc'

      this.webrtcDataChannel = this.webrtc.createDataChannel('webdisplay', {
        ordered: true
      })
      this.webrtcDataChannel.binaryType = 'arraybuffer'

      return new Promise((resolve, reject) => {
        const timeoutTimer = setTimeout(() => {
          this._resetTransportState()
          reject(new Error('WebRTC 连接超时'))
        }, 3000)

        this.webrtcDataChannel.onopen = () => {
          clearTimeout(timeoutTimer)
          if (this.transportKind !== 'webrtc') return

          console.log('WebRTC DataChannel 已连接')
          this.connected = true
          this.autoReconnect = true
          this._setConnectionState({
            visible: false,
            connected: true,
            text: '已连接',
            detail: '传输: WebRTC',
          })
          this._syncEncodingSettings(false)
          this._requestKeyframe()
          resolve(true)
        }

        this.webrtcDataChannel.onmessage = (event) => {
          if (event.data instanceof ArrayBuffer) {
            this._handleMessage(new Uint8Array(event.data))
          }
        }

        this.webrtcDataChannel.onclose = () => {
          if (this.transportKind === 'webrtc') {
            console.log('WebRTC DataChannel 已断开')
            this._onTransportDisconnected('WebRTC 连接断开')
          }
        }

        this.webrtc.onicecandidate = (e) => {
          if (e.candidate) {
            console.log('Got Local ICE Candidate', e.candidate)
          } else {
            // ICE gathering finished
            fetch(WEBRTC_OFFER_PATH, {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ sdp: this.webrtc.localDescription.sdp })
            })
              .then(response => {
                if (!response.ok) throw new Error('WebRTC signaling failed')
                return response.json()
              })
              .then(data => {
                return this.webrtc.setRemoteDescription(new RTCSessionDescription({
                  type: 'answer',
                  sdp: data.sdp
                }))
              })
              .catch(error => {
                clearTimeout(timeoutTimer)
                console.warn('WebRTC 握手失败:', error)
                if (this.webrtc) {
                  try { this.webrtc.close() } catch (_) { }
                }
                this._resetTransportState()
                resolve(false)
              })
          }
        }

        this.webrtc.onconnectionstatechange = () => {
          console.log('WebRTC state:', this.webrtc.connectionState)
          if (['failed', 'disconnected', 'closed'].includes(this.webrtc.connectionState)) {
            if (this.transportKind === 'webrtc') {
              this._onTransportDisconnected('WebRTC 连接断开')
            }
          }
        }

        this.webrtc.createOffer()
          .then(offer => this.webrtc.setLocalDescription(offer))
          .catch(error => {
            console.warn('WebRTC createOffer 失败:', error)
          })
      })

    } catch (e) {
      console.warn('WebRTC 初始化失败:', e)
      return false
    }
  }

  async _fetchWebTransportServerCertificateHash(timeoutMs = 1200) {
    const endpoint = `${location.origin}${WEBTRANSPORT_HASH_PATH}`
    const controller = new AbortController()
    const timer = setTimeout(() => {
      controller.abort()
    }, timeoutMs)

    try {
      const response = await fetch(endpoint, {
        method: 'GET',
        cache: 'no-store',
        signal: controller.signal,
      })
      if (!response.ok) {
        return null
      }

      const payload = await response.json()
      const bytes = payload && Array.isArray(payload.value) ? payload.value : null
      if (!bytes || bytes.length !== 32) {
        return null
      }

      const normalizedBytes = []
      for (const raw of bytes) {
        const value = Number.parseInt(raw, 10)
        if (!Number.isFinite(value) || value < 0 || value > 255) {
          return null
        }
        normalizedBytes.push(value)
      }

      return {
        algorithm: typeof payload.algorithm === 'string' ? payload.algorithm.toLowerCase() : 'sha-256',
        value: new Uint8Array(normalizedBytes),
      }
    } catch (_) {
      return null
    } finally {
      clearTimeout(timer)
    }
  }

  _connectWebSocket() {
    const wsProtocol = location.protocol === 'https:' ? 'wss' : 'ws'
    const wsUrl = `${wsProtocol}://${location.host}/ws`
    console.log('连接到:', wsUrl)

    this.transportKind = 'websocket'
    this.ws = new WebSocket(wsUrl)
    this.ws.binaryType = 'arraybuffer'

    this.ws.onopen = () => {
      if (this.transportKind !== 'websocket') {
        return
      }

      console.log('WebSocket 已连接')
      this.connected = true
      this.autoReconnect = true
      this._setConnectionState({
        visible: false,
        connected: true,
        text: '已连接',
        detail: '传输: WebSocket',
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
      if (this.transportKind === 'websocket') {
        console.log('WebSocket 已断开')
        this._onTransportDisconnected('WebSocket 连接断开')
      }
    }

    this.ws.onerror = (error) => {
      console.error('WebSocket 错误:', error)
    }
  }

  async _webTransportReadLoop() {
    if (!this.webTransportReader) {
      return
    }

    try {
      while (this.transportKind === 'webtransport' && this.webTransportReader) {
        const { value, done } = await this.webTransportReader.read()
        if (done) {
          break
        }

        if (!value || value.byteLength === 0) {
          continue
        }

        this._consumeWebTransportChunk(value)
      }
    } catch (error) {
      if (this.transportKind === 'webtransport') {
        console.warn('WebTransport 读取失败:', error)
      }
    }

    if (this.transportKind === 'webtransport') {
      this._onTransportDisconnected('WebTransport 连接断开')
    }
  }

  _consumeWebTransportChunk(chunk) {
    const incoming = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk)
    const merged = new Uint8Array(this.webTransportReadBuffer.byteLength + incoming.byteLength)
    merged.set(this.webTransportReadBuffer, 0)
    merged.set(incoming, this.webTransportReadBuffer.byteLength)
    this.webTransportReadBuffer = merged

    while (this.webTransportReadBuffer.byteLength >= 4) {
      const frameLen = new DataView(
        this.webTransportReadBuffer.buffer,
        this.webTransportReadBuffer.byteOffset,
        4,
      ).getUint32(0, true)

      if (frameLen > MAX_WEBTRANSPORT_PACKET_SIZE) {
        throw new Error(`WebTransport 包长度异常: ${frameLen}`)
      }

      const packetLen = 4 + frameLen
      if (this.webTransportReadBuffer.byteLength < packetLen) {
        return
      }

      const packet = this.webTransportReadBuffer.slice(4, packetLen)
      const packetBuffer = packet.buffer.slice(packet.byteOffset, packet.byteOffset + packet.byteLength)
      this._handleMessage(packetBuffer)

      this.webTransportReadBuffer = this.webTransportReadBuffer.slice(packetLen)
    }
  }

  _clampInt(rawValue, min, max, fallback) {
    const value = Number.parseInt(rawValue, 10)
    if (!Number.isFinite(value)) {
      return fallback
    }
    return Math.min(Math.max(value, min), max)
  }

  _normalizeCodecId(rawCodec, fallbackCodec) {
    if (typeof rawCodec !== 'string') {
      return fallbackCodec
    }

    const raw = rawCodec.trim().toLowerCase()
    const nextCodec = CODEC_ID_ALIASES[raw] || raw
    if (this.supportedCodecConfigs.size === 0) {
      return CODEC_PRESETS.some((item) => item.id === nextCodec) ? nextCodec : fallbackCodec
    }

    return this.supportedCodecConfigs.has(nextCodec) ? nextCodec : fallbackCodec
  }

  _normalizeEncodingDraft(draft, fallback = ENCODING_DEFAULTS) {
    const source = draft && typeof draft === 'object' ? draft : {}
    const fallbackCodec = this._normalizeCodecId(fallback.codec, ENCODING_DEFAULTS.codec)

    return {
      codec: this._normalizeCodecId(source.codec, fallbackCodec),
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
    this.ui.encodingDraft = {
      ...ENCODING_DEFAULTS,
      codec: this.encodingSettings.codec,
    }
    this._applyEncodingSettings()
  }

  selectMonitor(index) {
    this._requestMonitorSwitch(index)
  }

  _syncEncodingSettings(requestKeyframe = true) {
    if (!this._isTransportOpen()) {
      return false
    }

    this._sendJsonControlPacket(FRAME_TYPE.ENCODING_SETTINGS, {
      codec: this.encodingSettings.codec,
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
    const highFpsAv1 = normalized.codec === 'av1' && normalized.fps >= HIGH_FPS_HINT_THRESHOLD
    const codecChanged = normalized.codec !== this.encodingSettings.codec

    this.encodingSettings = { ...normalized }
    this.ui.encodingDraft = { ...normalized }

    const syncOk = this._syncEncodingSettings(true)
    const codecLabel = this._codecLabel(this.encodingSettings.codec)
    if (syncOk) {
      this._flashHint(
        `编码设置已应用: ${codecLabel} / ${this.encodingSettings.fps}fps / ${this.encodingSettings.bitrateMbps}Mbps`,
      )
      if (highFpsAv1) {
        setTimeout(() => {
          this._flashHint('当前 AV1 高帧率可能受限，建议切换 AVC/HEVC 以提升 FPS')
        }, 900)
      }
    } else {
      this._flashHint('编码设置已保存，将在重连后自动应用')
    }
  }

  _applyServerEncodingSettings(payload) {
    const bitrateMbps = Number.isFinite(payload?.bitrate)
      ? Math.round(Number(payload.bitrate) / 1_000_000)
      : undefined

    const normalized = this._normalizeEncodingDraft(
      {
        codec: payload?.codec,
        fps: payload?.fps,
        bitrateMbps,
        keyframeInterval: payload?.keyframe_interval,
      },
      this.encodingSettings,
    )

    this.encodingSettings = { ...normalized }
    this.ui.encodingDraft = { ...normalized }

    if (normalized.codec !== this.activeDecoderCodecId) {
      this._switchDecoder(normalized.codec)
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
    if (this._isTransportActive()) {
      this.autoReconnect = false
      this._deactivateControl()
      this._closeTransport('client quit')
      this._resetTransportState()
      this.connected = false
      this._setConnectionState({
        visible: true,
        connected: false,
        text: '会话已退出，按 Ctrl+Alt+Shift+Q 重连',
        detail: '',
      })
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
    this.transportCloseHandled = false
    void this._connect()
    this._flashHint('正在重新连接')
  }

  _toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => { })
    } else {
      document.documentElement.requestFullscreen().catch(() => { })
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
    if (this._isTransportOpen()) {
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

  _getRenderRect() {
    const rect = this.canvas.getBoundingClientRect()
    if (rect.width <= 0 || rect.height <= 0 || this.canvas.width <= 0 || this.canvas.height <= 0) {
      return rect
    }

    const canvasRatio = this.canvas.width / this.canvas.height
    const rectRatio = rect.width / rect.height

    let drawWidth = rect.width
    let drawHeight = rect.height
    let offsetX = 0
    let offsetY = 0

    if (canvasRatio > rectRatio) {
      drawHeight = rect.width / canvasRatio
      offsetY = (rect.height - drawHeight) / 2
    } else {
      drawWidth = rect.height * canvasRatio
      offsetX = (rect.width - drawWidth) / 2
    }

    return {
      left: rect.left + offsetX,
      top: rect.top + offsetY,
      width: drawWidth,
      height: drawHeight,
    }
  }

  _normalizePointer(clientX, clientY) {
    if (typeof clientX !== 'number' || typeof clientY !== 'number') {
      return this.lastPointerPos
    }

    const rect = this._getRenderRect()
    if (rect.width <= 0 || rect.height <= 0) {
      return this.lastPointerPos
    }

    const x = Math.min(Math.max((clientX - rect.left) / rect.width, 0), 1)
    const y = Math.min(Math.max((clientY - rect.top) / rect.height, 0), 1)
    return { x, y }
  }

  _normalizePointerDelta(movementX, movementY) {
    const rect = this._getRenderRect()
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

  _sendBinaryPacket(buffer) {
    if (!this._isTransportOpen()) {
      return false
    }

    if (this.transportKind === 'websocket' && this.ws) {
      this.ws.send(buffer)
      return true
    }

    if (this.transportKind === 'webtransport' && this.webTransportWriter) {
      let payload = null
      if (buffer instanceof ArrayBuffer) {
        payload = new Uint8Array(buffer)
      } else if (ArrayBuffer.isView(buffer)) {
        payload = new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength)
      }

      if (!payload) {
        return false
      }

      const framed = new Uint8Array(4 + payload.byteLength)
      new DataView(framed.buffer).setUint32(0, payload.byteLength, true)
      framed.set(payload, 4)

      this.webTransportWriter.write(framed).catch((error) => {
        console.warn('WebTransport 写入失败:', error)
        if (this.transportKind === 'webtransport') {
          this._onTransportDisconnected('WebTransport 写入失败')
        }
      })
      return true
    }

    if (this.transportKind === 'webrtc' && this.webrtcDataChannel && this.webrtcDataChannel.readyState === 'open') {
      try {
        this.webrtcDataChannel.send(buffer)
        return true
      } catch (e) {
        console.warn('WebRTC 发送失败:', e)
        return false
      }
    }

    return false
  }

  _sendJsonControlPacket(frameType, payloadObj) {
    if (!this._isTransportOpen()) {
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
    this._sendBinaryPacket(packet.buffer)
  }

  _handleMessage(data) {
    if (this.transportKind === 'webrtc') {
      // Reassemble WebRTC chunked packets
      // format: [4 bytes total_len][4 bytes offset][chunk_data]
      const dataView = new DataView(data.buffer, data.byteOffset, data.byteLength)
      if (data.byteLength < 8) return;
      const totalLen = dataView.getUint32(0, true)
      const offset = dataView.getUint32(4, true)

      if (offset === 0) {
        if (totalLen === data.byteLength - 8) {
          // Single unfragmented packet
          this.__handleCompleteMessage(new Uint8Array(data.buffer, data.byteOffset + 8, data.byteLength - 8))
          return
        }
        this.webrtcChunkBuffer = new Uint8Array(totalLen)
      }

      if (this.webrtcChunkBuffer) {
        this.webrtcChunkBuffer.set(new Uint8Array(data.buffer, data.byteOffset + 8, data.byteLength - 8), offset)
        if (offset + (data.byteLength - 8) >= totalLen) {
          const completePacket = this.webrtcChunkBuffer
          this.webrtcChunkBuffer = null
          this.__handleCompleteMessage(completePacket)
        }
      }
      return
    }

    this.__handleCompleteMessage(data)
  }

  __handleCompleteMessage(data) {
    if (data.byteLength < HEADER_SIZE) {
      return
    }

    const view = new DataView(data.buffer, data.byteOffset, data.byteLength)
    const frameType = view.getUint8(0)
    const flags = view.getUint8(1)
    const sequence = view.getUint32(2, true)
    const payloadLen = view.getUint32(10, true)

    if (HEADER_SIZE + payloadLen > data.byteLength) {
      return
    }

    const payload = new Uint8Array(data.buffer, data.byteOffset + HEADER_SIZE, payloadLen)

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

    if (frameType === FRAME_TYPE.ENCODING_SETTINGS) {
      try {
        const jsonStr = this.textDecoder.decode(payload)
        const settings = JSON.parse(jsonStr)
        this._applyServerEncodingSettings(settings)
      } catch (error) {
        console.error('解析编码设置失败', error)
      }
      return
    }

    if (frameType !== FRAME_TYPE.VIDEO) {
      return
    }

    const isKeyframe = (flags & FRAME_FLAGS.KEYFRAME) !== 0
    this.stats.bitrateBytes += data.byteLength

    if (this.awaitingKeyframe && !isKeyframe) {
      return
    }
    if (isKeyframe) {
      this.awaitingKeyframe = false
    }

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
        timestamp: this._nextChunkTimestampUs(sequence),
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
    const codecId = this.activeDecoderCodecId || this.encodingSettings.codec
    this._initDecoder(codecId)
  }

  _nextChunkTimestampUs(sequence) {
    const fps = this._clampInt(
      this.encodingSettings.fps,
      ENCODING_LIMITS.fps.min,
      ENCODING_LIMITS.fps.max,
      ENCODING_DEFAULTS.fps,
    )
    const frameDurationUs = Math.max(1, Math.round(1_000_000 / fps))
    const candidateTimestampUs = sequence * frameDurationUs
    const nextTimestampUs =
      candidateTimestampUs > this.lastChunkTimestampUs
        ? candidateTimestampUs
        : this.lastChunkTimestampUs + frameDurationUs

    this.lastChunkTimestampUs = nextTimestampUs
    return nextTimestampUs
  }

  _requestKeyframe() {
    if (!this._isTransportOpen()) {
      return
    }

    const header = new ArrayBuffer(HEADER_SIZE)
    const view = new DataView(header)
    view.setUint8(0, FRAME_TYPE.KEYFRAME_REQUEST)
    this._sendBinaryPacket(header)
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
    if (!this._isTransportOpen()) {
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

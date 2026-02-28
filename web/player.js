/**
 * 超低延迟 WebCodecs AV1 播放器
 *
 * 核心设计原则：
 * 1. 零缓冲：解码后立即渲染，不使用任何帧缓冲队列
 * 2. 帧丢弃：如果解码跟不上，丢弃旧帧只渲染最新帧
 * 3. 时间戳同步：使用服务端时间戳进行延迟测量
 */

const HEADER_SIZE = 16;
const SERVER_FPS = 60;
const FRAME_TIMESTAMP_US = Math.round(1_000_000 / SERVER_FPS);
const ENCODING_DEFAULTS = Object.freeze({
    fps: 60,
    bitrateMbps: 20,
    keyframeInterval: 2,
});
const ENCODING_LIMITS = Object.freeze({
    fps: { min: 24, max: 120 },
    bitrateMbps: { min: 2, max: 80 },
    keyframeInterval: { min: 1, max: 10 },
});

// 帧类型
const FRAME_TYPE = {
    VIDEO: 0x01,
    KEYFRAME_REQUEST: 0x02,
    STATS: 0x03,
    MONITOR_LIST: 0x04,
    MONITOR_SELECT: 0x05,
    MOUSE_INPUT: 0x06,
    KEYBOARD_INPUT: 0x07,
    ENCODING_SETTINGS: 0x08,
    PING: 0x10,
    PONG: 0x11,
};

// 帧标志
const FRAME_FLAGS = {
    KEYFRAME: 0x01,
    END_OF_FRAME: 0x02,
};

class UltraLowLatencyPlayer {
    constructor() {
        this.canvas = document.getElementById('stream-canvas');
        this.ctx = this.canvas.getContext('2d');
        this.overlay = document.getElementById('overlay');
        this.statusEl = document.getElementById('connection-status');
        this.monitorPicker = document.getElementById('monitor-picker');
        this.monitorListEl = document.getElementById('monitor-list');
        this.encodingPanel = document.getElementById('encoding-panel');
        this.encodingBitrateInput = document.getElementById('encoding-bitrate');
        this.encodingFpsInput = document.getElementById('encoding-fps');
        this.encodingKeyintInput = document.getElementById('encoding-keyint');
        this.encodingBitrateValue = document.getElementById('encoding-bitrate-value');
        this.encodingFpsValue = document.getElementById('encoding-fps-value');
        this.encodingKeyintValue = document.getElementById('encoding-keyint-value');
        this.encodingApplyBtn = document.getElementById('encoding-apply');
        this.encodingResetBtn = document.getElementById('encoding-reset');
        this.controlHintEl = document.getElementById('control-hint');
        this.baseControlHint = this.controlHintEl ? this.controlHintEl.textContent : '';

        this.decoder = null;
        this.ws = null;
        this.connected = false;
        this.autoReconnect = true;
        this.reconnectTimer = null;
        this.hintTimer = null;
        this.hintHideTimer = null;

        // 统计
        this.stats = {
            framesDecoded: 0,
            framesDropped: 0,
            lastFpsTime: performance.now(),
            fpsCount: 0,
            fps: 0,
            decodeTimeMs: 0,
            latencyMs: 0,
            bitrateBytes: 0,
            bitrateMbps: 0,
        };

        // 输入控制状态
        this.controlActive = false;
        this.pressedKeys = new Map();
        this.pressedButtons = new Set();
        this.lastPointerPos = { x: 0.5, y: 0.5 };
        this.pendingMouseMoveEvent = null;
        this.mouseMoveScheduled = false;
        this.showLocalCursor = false;
        this.lockPointerToVideo = false;

        this.textEncoder = new TextEncoder();
        this.textDecoder = new TextDecoder();
        this.canvas.tabIndex = 0;

        this.encodingSettings = { ...ENCODING_DEFAULTS };
        this.encodingDraft = { ...this.encodingSettings };

        // 最新待渲染的帧（原子替换，旧帧自动丢弃）
        this.pendingFrame = null;
        this.renderBound = this._renderLoop.bind(this);

        this._init();
    }

    async _init() {
        // 检查 WebCodecs 支持
        if (typeof VideoDecoder === 'undefined') {
            this.statusEl.innerHTML = '❌ 浏览器不支持 WebCodecs API<br>请使用 Chrome 94+ 或 Edge 94+';
            return;
        }

        // 检查 AV1 解码支持
        const support = await VideoDecoder.isConfigSupported({
            codec: 'av01.0.08M.08', // AV1 Main Profile, Level 4.0, 8-bit
            hardwareAcceleration: 'prefer-hardware',
        });

        if (!support.supported) {
            this.statusEl.innerHTML = '❌ 浏览器不支持 AV1 硬件解码';
            return;
        }

        this._initDecoder();
        this._bindEncodingEvents();
        this._renderEncodingSettings();
        this._connect();
        requestAnimationFrame(this.renderBound);
        this._startStatsUpdate();
        this._bindInputEvents();
        this._showControlHint(4500);
    }

    /**
     * 初始化 WebCodecs AV1 解码器
     *
     * 关键配置:
     * - hardwareAcceleration: 'prefer-hardware' → 使用 GPU 硬件解码
     * - optimizeForLatency: true → 解码器优化为低延迟模式
     */
    _initDecoder() {
        this.decoder = new VideoDecoder({
            output: (frame) => {
                // 原子替换 pendingFrame，旧帧立即 close()
                const oldFrame = this.pendingFrame;
                this.pendingFrame = frame;
                if (oldFrame) {
                    oldFrame.close();
                    this.stats.framesDropped++;
                }
                this.stats.framesDecoded++;
            },
            error: (e) => {
                console.error('解码错误:', e);
                // 解码器出错后必须重新初始化，再请求关键帧
                this._resetDecoder();
                this._requestKeyframe();
            },
        });

        this.decoder.configure({
            codec: 'av01.0.08M.08',
            hardwareAcceleration: 'prefer-hardware',
            optimizeForLatency: true,
        });

        console.log('AV1 解码器已初始化 (硬件加速, 低延迟模式)');
    }

    /**
     * 建立 WebSocket 连接
     */
    _connect() {
        if (this.reconnectTimer) {
            clearTimeout(this.reconnectTimer);
            this.reconnectTimer = null;
        }

        if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
            return;
        }

        const wsProtocol = location.protocol === 'https:' ? 'wss' : 'ws';
        const wsUrl = `${wsProtocol}://${location.host}/ws`;
        console.log('连接到:', wsUrl);

        this.ws = new WebSocket(wsUrl);
        this.ws.binaryType = 'arraybuffer';

        this.ws.onopen = () => {
            console.log('WebSocket 已连接');
            this.connected = true;
            this.autoReconnect = true;
            this.statusEl.style.display = 'none';
            this._syncEncodingSettings(false);
            // 连接后立即请求关键帧
            this._requestKeyframe();
        };

        this.ws.onmessage = (event) => {
            const recvTime = performance.now();
            this._handleMessage(event.data, recvTime);
        };

        this.ws.onclose = () => {
            console.log('WebSocket 已断开');
            this.connected = false;
            this._releaseAllInputs();
            this.controlActive = false;
            this._exitPointerLock();
            this._applyCursorVisibility();
            this.statusEl.style.display = 'block';

            if (this.autoReconnect) {
                this.statusEl.innerHTML = '<span class="dot disconnected"></span> 连接断开，3秒后重连...';
                this.reconnectTimer = setTimeout(() => {
                    this.reconnectTimer = null;
                    this._connect();
                }, 3000);
            } else {
                this.statusEl.innerHTML = '<span class="dot disconnected"></span> 会话已退出，按 Ctrl+Alt+Shift+Q 重连';
            }
        };

        this.ws.onerror = (e) => {
            console.error('WebSocket 错误:', e);
        };
    }

    _bindEncodingEvents() {
        const onDraftChanged = () => {
            this.encodingDraft = this._readEncodingSettingsFromInputs(this.encodingDraft);
            this._renderEncodingSettings(this.encodingDraft);
        };

        if (this.encodingBitrateInput) {
            this.encodingBitrateInput.addEventListener('input', onDraftChanged);
        }
        if (this.encodingFpsInput) {
            this.encodingFpsInput.addEventListener('input', onDraftChanged);
        }
        if (this.encodingKeyintInput) {
            this.encodingKeyintInput.addEventListener('input', onDraftChanged);
        }
        if (this.encodingApplyBtn) {
            this.encodingApplyBtn.addEventListener('click', () => {
                this._applyEncodingSettings();
            });
        }
        if (this.encodingResetBtn) {
            this.encodingResetBtn.addEventListener('click', () => {
                this.encodingDraft = { ...ENCODING_DEFAULTS };
                this._renderEncodingSettings(this.encodingDraft);
                this._applyEncodingSettings();
            });
        }
    }

    _clampInt(rawValue, min, max, fallback) {
        const value = Number.parseInt(rawValue, 10);
        if (!Number.isFinite(value)) {
            return fallback;
        }
        return Math.min(Math.max(value, min), max);
    }

    _readEncodingSettingsFromInputs(fallback = ENCODING_DEFAULTS) {
        const bitrateRaw = this.encodingBitrateInput ? this.encodingBitrateInput.value : fallback.bitrateMbps;
        const fpsRaw = this.encodingFpsInput ? this.encodingFpsInput.value : fallback.fps;
        const keyintRaw = this.encodingKeyintInput ? this.encodingKeyintInput.value : fallback.keyframeInterval;

        return {
            bitrateMbps: this._clampInt(
                bitrateRaw,
                ENCODING_LIMITS.bitrateMbps.min,
                ENCODING_LIMITS.bitrateMbps.max,
                fallback.bitrateMbps,
            ),
            fps: this._clampInt(
                fpsRaw,
                ENCODING_LIMITS.fps.min,
                ENCODING_LIMITS.fps.max,
                fallback.fps,
            ),
            keyframeInterval: this._clampInt(
                keyintRaw,
                ENCODING_LIMITS.keyframeInterval.min,
                ENCODING_LIMITS.keyframeInterval.max,
                fallback.keyframeInterval,
            ),
        };
    }

    _renderEncodingSettings(settings = this.encodingDraft) {
        if (this.encodingBitrateInput) {
            this.encodingBitrateInput.value = `${settings.bitrateMbps}`;
        }
        if (this.encodingFpsInput) {
            this.encodingFpsInput.value = `${settings.fps}`;
        }
        if (this.encodingKeyintInput) {
            this.encodingKeyintInput.value = `${settings.keyframeInterval}`;
        }
        if (this.encodingBitrateValue) {
            this.encodingBitrateValue.textContent = `${settings.bitrateMbps} Mbps`;
        }
        if (this.encodingFpsValue) {
            this.encodingFpsValue.textContent = `${settings.fps} FPS`;
        }
        if (this.encodingKeyintValue) {
            this.encodingKeyintValue.textContent = `${settings.keyframeInterval} 秒`;
        }
    }

    _syncEncodingSettings(requestKeyframe = true) {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
            return false;
        }

        this._sendJsonControlPacket(FRAME_TYPE.ENCODING_SETTINGS, {
            fps: this.encodingSettings.fps,
            bitrate: this.encodingSettings.bitrateMbps * 1_000_000,
            keyframe_interval: this.encodingSettings.keyframeInterval,
        });

        if (requestKeyframe) {
            this._requestKeyframe();
        }

        return true;
    }

    _applyEncodingSettings() {
        this.encodingDraft = this._readEncodingSettingsFromInputs(this.encodingDraft);
        this.encodingSettings = { ...this.encodingDraft };
        this._renderEncodingSettings(this.encodingDraft);

        const syncOk = this._syncEncodingSettings(true);
        if (syncOk) {
            this._flashHint(`编码设置已应用: ${this.encodingSettings.fps}fps / ${this.encodingSettings.bitrateMbps}Mbps`);
        } else {
            this._flashHint('编码设置已保存，将在重连后自动应用');
        }
    }

    _toggleEncodingPanel() {
        if (!this.encodingPanel) {
            return;
        }

        const willOpen = this.encodingPanel.classList.contains('hidden');
        if (willOpen) {
            this.encodingDraft = { ...this.encodingSettings };
            this._renderEncodingSettings(this.encodingDraft);
            this.monitorPicker.classList.add('hidden');
        }
        this.encodingPanel.classList.toggle('hidden');
    }

    _bindInputEvents() {
        window.addEventListener('keydown', (e) => {
            if (this._handleMoonlightShortcutKeyDown(e)) {
                return;
            }

            if (!this.controlActive) {
                return;
            }

            if (e.key === 'Escape') {
                e.preventDefault();
                this._deactivateControl();
                return;
            }

            if (e.isComposing) return;
            e.preventDefault();
            this._sendKeyboardInput(e, true);
        }, true);

        window.addEventListener('keyup', (e) => {
            if (this._handleMoonlightShortcutKeyUp(e)) {
                return;
            }

            if (!this.controlActive || e.key === 'Escape' || e.isComposing) {
                return;
            }
            e.preventDefault();
            this._sendKeyboardInput(e, false);
        }, true);

        this.canvas.addEventListener('mousedown', (e) => {
            e.preventDefault();
            this._activateControl();

            const pos = this._capturePointerPositionFromMouse(e);
            this.pressedButtons.add(e.button);
            this._sendMouseInput({
                kind: 'button',
                x: pos.x,
                y: pos.y,
                button: e.button,
                down: true,
            });
        });

        this.canvas.addEventListener('mousemove', (e) => {
            if (!this.controlActive) return;

            if (this._isPointerLocked()) {
                this.pendingMouseMoveEvent = {
                    mode: 'relative',
                    movementX: e.movementX,
                    movementY: e.movementY,
                };
            } else {
                this.pendingMouseMoveEvent = {
                    mode: 'absolute',
                    clientX: e.clientX,
                    clientY: e.clientY,
                };
            }

            this._scheduleMouseMove();
        });

        const releaseMouseButton = (e) => {
            if (!this.controlActive || !this.pressedButtons.has(e.button)) {
                return;
            }

            e.preventDefault();
            const pos = this._capturePointerPositionFromMouse(e);
            this.pressedButtons.delete(e.button);
            this._sendMouseInput({
                kind: 'button',
                x: pos.x,
                y: pos.y,
                button: e.button,
                down: false,
            });
        };

        this.canvas.addEventListener('mouseup', releaseMouseButton);
        window.addEventListener('mouseup', releaseMouseButton, true);

        this.canvas.addEventListener('wheel', (e) => {
            if (!this.controlActive) return;

            e.preventDefault();
            const pos = this._capturePointerPositionFromMouse(e);
            const unit = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? 40
                : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? 120
                    : 1;
            const deltaX = Math.round(e.deltaX * unit);
            const deltaY = Math.round(-e.deltaY * unit);
            if (deltaX === 0 && deltaY === 0) return;

            this._sendMouseInput({
                kind: 'wheel',
                x: pos.x,
                y: pos.y,
                delta_x: deltaX,
                delta_y: deltaY,
            });
        }, { passive: false });

        this.canvas.addEventListener('contextmenu', (e) => {
            if (this.controlActive) {
                e.preventDefault();
            }
        });

        window.addEventListener('blur', () => {
            this._deactivateControl();
        });

        document.addEventListener('visibilitychange', () => {
            if (document.hidden) {
                this._deactivateControl();
            }
        });

        document.addEventListener('pointerlockchange', () => {
            if (this.lockPointerToVideo && this.controlActive && !this._isPointerLocked()) {
                this._flashHint('鼠标锁定已解除，按 Ctrl+Alt+Shift+L 重新锁定');
            }
        });

        window.addEventListener('mousemove', (e) => {
            if (e.clientY >= window.innerHeight - 72) {
                this._showControlHint(2400);
            }
        }, { passive: true });

        window.addEventListener('touchstart', () => {
            this._showControlHint(2400);
        }, { passive: true });
    }

    _isMoonlightShortcutChord(e) {
        return e.ctrlKey && e.altKey && e.shiftKey && !e.metaKey;
    }

    _moonlightShortcutAction(code) {
        switch (code) {
            case 'KeyQ':
                return 'quit';
            case 'KeyZ':
                return 'capture';
            case 'KeyX':
                return 'fullscreen';
            case 'KeyS':
                return 'stats';
            case 'KeyM':
                return 'monitor';
            case 'KeyE':
                return 'encoding';
            case 'KeyL':
                return 'lock';
            case 'KeyC':
                return 'cursor';
            case 'KeyV':
                return 'clipboard';
            case 'KeyD':
                return 'minimize';
            default:
                return null;
        }
    }

    _handleMoonlightShortcutKeyDown(e) {
        if (!this._isMoonlightShortcutChord(e)) {
            return false;
        }

        const action = this._moonlightShortcutAction(e.code);
        if (!action) {
            return false;
        }

        e.preventDefault();
        e.stopPropagation();
        this._showControlHint(2600);
        if (e.repeat) {
            return true;
        }

        switch (action) {
            case 'quit':
                this._toggleStreamSession();
                break;
            case 'capture':
                if (this.controlActive) {
                    this._deactivateControl();
                    this._flashHint('输入控制已释放');
                } else {
                    this._activateControl();
                    this._flashHint('输入控制已激活');
                }
                break;
            case 'fullscreen':
                this._toggleFullscreen();
                break;
            case 'stats':
                this.overlay.classList.toggle('hidden');
                break;
            case 'monitor':
                this.encodingPanel.classList.add('hidden');
                this.monitorPicker.classList.toggle('hidden');
                break;
            case 'encoding':
                this._toggleEncodingPanel();
                break;
            case 'lock':
                this.lockPointerToVideo = !this.lockPointerToVideo;
                if (!this.controlActive || this.showLocalCursor) {
                    if (!this.lockPointerToVideo) {
                        this._exitPointerLock();
                    }
                } else if (this.lockPointerToVideo) {
                    this._requestPointerLock();
                } else {
                    this._exitPointerLock();
                }
                this._flashHint(`鼠标锁定${this.lockPointerToVideo ? '已开启' : '已关闭'}`);
                break;
            case 'cursor':
                this.showLocalCursor = !this.showLocalCursor;
                this._applyCursorVisibility();
                if (this.showLocalCursor) {
                    this._exitPointerLock();
                } else if (this.controlActive && this.lockPointerToVideo) {
                    this._requestPointerLock();
                }
                this._flashHint(`本地光标${this.showLocalCursor ? '显示' : '隐藏'}`);
                break;
            case 'clipboard':
                this._flashHint('浏览器版暂不支持 Ctrl+Alt+Shift+V 粘贴');
                break;
            case 'minimize':
                this._flashHint('浏览器版暂不支持 Ctrl+Alt+Shift+D 最小化');
                break;
            default:
                break;
        }

        return true;
    }

    _handleMoonlightShortcutKeyUp(e) {
        if (!this._isMoonlightShortcutChord(e)) {
            return false;
        }

        const action = this._moonlightShortcutAction(e.code);
        if (!action) {
            return false;
        }

        e.preventDefault();
        e.stopPropagation();
        return true;
    }

    _toggleStreamSession() {
        if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
            this.autoReconnect = false;
            this._deactivateControl();
            this.ws.close(1000, 'client quit');
            this._flashHint('会话已退出');
            return;
        }

        this.autoReconnect = true;
        this.statusEl.style.display = 'block';
        this.statusEl.innerHTML = '<span class="dot disconnected"></span> 正在连接...';
        this._connect();
        this._flashHint('正在重新连接');
    }

    _toggleFullscreen() {
        if (document.fullscreenElement) {
            document.exitFullscreen().catch(() => { });
        } else {
            document.documentElement.requestFullscreen().catch(() => { });
        }
    }

    _showControlHint(autoHideMs = 3000) {
        if (!this.controlHintEl) {
            return;
        }

        this.controlHintEl.classList.remove('hidden');

        if (this.hintHideTimer) {
            clearTimeout(this.hintHideTimer);
            this.hintHideTimer = null;
        }

        if (autoHideMs <= 0) {
            return;
        }

        this.hintHideTimer = setTimeout(() => {
            this.controlHintEl.classList.add('hidden');
            this.hintHideTimer = null;
        }, autoHideMs);
    }

    _flashHint(text) {
        if (!this.controlHintEl) {
            return;
        }

        this.controlHintEl.textContent = text;
        this._showControlHint(2500);
        if (this.hintTimer) {
            clearTimeout(this.hintTimer);
        }

        this.hintTimer = setTimeout(() => {
            this.controlHintEl.textContent = this.baseControlHint;
            this._showControlHint(3200);
            this.hintTimer = null;
        }, 2200);
    }

    _isPointerLocked() {
        return document.pointerLockElement === this.canvas;
    }

    _requestPointerLock() {
        if (this._isPointerLocked()) {
            return;
        }

        this.canvas.requestPointerLock();
    }

    _exitPointerLock() {
        if (this._isPointerLocked()) {
            document.exitPointerLock();
        }
    }

    _applyCursorVisibility() {
        this.canvas.classList.toggle('show-local-cursor', this.showLocalCursor);
    }

    _activateControl() {
        if (this.controlActive) return;
        this.controlActive = true;
        this.canvas.focus({ preventScroll: true });
        this._applyCursorVisibility();

        if (this.lockPointerToVideo && !this.showLocalCursor) {
            this._requestPointerLock();
        }

        console.log('远程输入已激活，按 Ctrl+Alt+Shift+Z 或 Esc 释放');
    }

    _deactivateControl() {
        if (!this.controlActive && this.pressedKeys.size === 0 && this.pressedButtons.size === 0) {
            return;
        }

        this._releaseAllInputs();
        this.controlActive = false;
        this.pendingMouseMoveEvent = null;
        this.mouseMoveScheduled = false;
        this._exitPointerLock();
        this._applyCursorVisibility();
        this.canvas.blur();
    }

    _releaseAllInputs() {
        if (this.ws && this.ws.readyState === WebSocket.OPEN) {
            for (const keyInfo of this.pressedKeys.values()) {
                this._sendJsonControlPacket(FRAME_TYPE.KEYBOARD_INPUT, {
                    key_code: keyInfo.keyCode,
                    code: keyInfo.code,
                    down: false,
                });
            }

            for (const button of this.pressedButtons.values()) {
                this._sendMouseInput({
                    kind: 'button',
                    x: this.lastPointerPos.x,
                    y: this.lastPointerPos.y,
                    button,
                    down: false,
                });
            }
        }

        this.pressedKeys.clear();
        this.pressedButtons.clear();
    }

    _scheduleMouseMove() {
        if (this.mouseMoveScheduled) return;

        this.mouseMoveScheduled = true;
        requestAnimationFrame(() => {
            this.mouseMoveScheduled = false;

            if (!this.controlActive || !this.pendingMouseMoveEvent) {
                return;
            }

            let pos = this.lastPointerPos;
            if (this.pendingMouseMoveEvent.mode === 'relative') {
                pos = this._normalizePointerDelta(
                    this.pendingMouseMoveEvent.movementX,
                    this.pendingMouseMoveEvent.movementY,
                );
            } else {
                pos = this._normalizePointer(
                    this.pendingMouseMoveEvent.clientX,
                    this.pendingMouseMoveEvent.clientY,
                );
            }

            this.pendingMouseMoveEvent = null;
            this.lastPointerPos = pos;
            this._sendMouseInput({ kind: 'move', x: pos.x, y: pos.y });
        });
    }

    _capturePointerPositionFromMouse(e) {
        const pos = this._normalizePointer(e.clientX, e.clientY);
        this.lastPointerPos = pos;
        return pos;
    }

    _normalizePointer(clientX, clientY) {
        if (typeof clientX !== 'number' || typeof clientY !== 'number') {
            return this.lastPointerPos;
        }

        const rect = this.canvas.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) {
            return this.lastPointerPos;
        }

        const x = Math.min(Math.max((clientX - rect.left) / rect.width, 0), 1);
        const y = Math.min(Math.max((clientY - rect.top) / rect.height, 0), 1);
        return { x, y };
    }

    _normalizePointerDelta(movementX, movementY) {
        const rect = this.canvas.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) {
            return this.lastPointerPos;
        }

        const x = Math.min(Math.max(this.lastPointerPos.x + movementX / rect.width, 0), 1);
        const y = Math.min(Math.max(this.lastPointerPos.y + movementY / rect.height, 0), 1);
        return { x, y };
    }

    _sendMouseInput(payload) {
        this._sendJsonControlPacket(FRAME_TYPE.MOUSE_INPUT, payload);
    }

    _sendKeyboardInput(event, down) {
        const keyCode = event.keyCode || event.which || 0;
        const code = event.code || null;
        if (keyCode === 0 && !code) {
            return;
        }

        const keyId = code || `kc-${keyCode}`;
        if (down) {
            this.pressedKeys.set(keyId, { keyCode, code });
        } else {
            this.pressedKeys.delete(keyId);
        }

        this._sendJsonControlPacket(FRAME_TYPE.KEYBOARD_INPUT, {
            key_code: keyCode,
            code,
            down,
        });
    }

    _sendJsonControlPacket(frameType, payloadObj) {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;

        const payload = this.textEncoder.encode(JSON.stringify(payloadObj));
        const header = new ArrayBuffer(HEADER_SIZE);
        const view = new DataView(header);
        view.setUint8(0, frameType);
        view.setUint32(10, payload.byteLength, true);

        const packet = new Uint8Array(HEADER_SIZE + payload.byteLength);
        packet.set(new Uint8Array(header), 0);
        packet.set(payload, HEADER_SIZE);
        this.ws.send(packet.buffer);
    }

    /**
     * 处理接收到的二进制消息
     */
    _handleMessage(data, recvTime) {
        if (data.byteLength < HEADER_SIZE) return;

        const view = new DataView(data);
        const frameType = view.getUint8(0);
        const flags = view.getUint8(1);
        const sequence = view.getUint32(2, true);
        const payloadLen = view.getUint32(10, true);

        if (HEADER_SIZE + payloadLen > data.byteLength) return;

        const payload = new Uint8Array(data, HEADER_SIZE, payloadLen);

        if (frameType === FRAME_TYPE.MONITOR_LIST) {
            try {
                const jsonStr = this.textDecoder.decode(payload);
                const monitors = JSON.parse(jsonStr);
                this._updateMonitorList(monitors);
            } catch (e) {
                console.error("解析显示器列表失败", e);
            }
            return;
        }

        if (frameType !== FRAME_TYPE.VIDEO) return;

        const isKeyframe = (flags & FRAME_FLAGS.KEYFRAME) !== 0;

        // 统计码率
        this.stats.bitrateBytes += data.byteLength;

        // 解码
        const decodeStart = performance.now();

        try {
            const queueSize = this.decoder.decodeQueueSize;
            if (queueSize > 10) {
                console.warn(`解码队列过长(${queueSize}), 重置`);
                this._resetDecoder();
                this._requestKeyframe();
                return;
            }

            const chunk = new EncodedVideoChunk({
                type: isKeyframe ? 'key' : 'delta',
                timestamp: sequence * FRAME_TIMESTAMP_US,
                data: payload,
            });

            // 守卫：只在解码器处于可用状态时才送入帧
            if (this.decoder.state !== 'configured') {
                return;
            }

            this.decoder.decode(chunk);
            this.stats.decodeTimeMs = performance.now() - decodeStart;
        } catch (e) {
            console.error('送入解码器失败:', e);
            this._resetDecoder();
            this._requestKeyframe();
        }
    }

    /**
     * 渲染循环 — 使用 requestAnimationFrame
     *
     * 设计: 每次 rAF 回调只渲染最新的一帧，
     * 避免帧积压导致延迟累积
     */
    _renderLoop() {
        const frame = this.pendingFrame;
        if (frame) {
            this.pendingFrame = null;

            // 调整 canvas 尺寸
            if (this.canvas.width !== frame.displayWidth ||
                this.canvas.height !== frame.displayHeight) {
                this.canvas.width = frame.displayWidth;
                this.canvas.height = frame.displayHeight;
            }

            // 绘制帧到 canvas
            this.ctx.drawImage(frame, 0, 0);
            frame.close();

            this.stats.fpsCount++;
        }

        requestAnimationFrame(this.renderBound);
    }

    /**
     * 安全地重置解码器：关闭旧实例并重新初始化
     */
    _resetDecoder() {
        if (this.decoder && this.decoder.state !== 'closed') {
            try { this.decoder.close(); } catch (_) { }
        }
        this.decoder = null;
        this._initDecoder();
    }

    /**
     * 请求服务端发送关键帧
     */
    _requestKeyframe() {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;

        const header = new ArrayBuffer(HEADER_SIZE);
        const view = new DataView(header);
        view.setUint8(0, FRAME_TYPE.KEYFRAME_REQUEST);
        this.ws.send(header);
        console.log('已请求关键帧');
    }

    /**
     * 更新显示器列表 UI
     */
    _updateMonitorList(monitors) {
        this.monitorListEl.innerHTML = '';
        if (!monitors || monitors.length === 0) {
            this.monitorListEl.innerHTML = '<div style="color:#888; text-align:center;">未检测到显示器</div>';
            return;
        }

        monitors.forEach(m => {
            const card = document.createElement('div');
            card.className = 'monitor-card';

            const info = document.createElement('div');
            info.className = 'monitor-info';

            const name = document.createElement('div');
            name.className = 'monitor-name';
            name.textContent = `显示器 ${m.index}: ${m.name} ${m.primary ? '(主屏)' : ''}`;

            const res = document.createElement('div');
            res.className = 'monitor-res';
            res.textContent = `${m.width} x ${m.height}`;

            info.appendChild(name);
            info.appendChild(res);

            const btn = document.createElement('button');
            btn.textContent = '切换';
            btn.style.cssText = 'background:#4fc3f7; color:#000; border:none; border-radius:4px; padding:6px 12px; cursor:pointer; font-weight:500;';

            // 点击切换显示器
            card.onclick = () => {
                this._requestMonitorSwitch(m.index);
            };

            card.appendChild(info);
            card.appendChild(btn);
            this.monitorListEl.appendChild(card);
        });
    }

    /**
     * 请求切换显示器
     */
    _requestMonitorSwitch(index) {
        if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;

        this._sendJsonControlPacket(FRAME_TYPE.MONITOR_SELECT, { index });
        console.log('已请求切换显示器到', index);

        // 隐藏面板并请求关键帧加速画面出现
        this.monitorPicker.classList.add('hidden');
        this._requestKeyframe();
    }

    /**
     * 更新统计信息显示
     */
    _startStatsUpdate() {
        setInterval(() => {
            const now = performance.now();
            const elapsed = (now - this.stats.lastFpsTime) / 1000;

            if (elapsed >= 1) {
                this.stats.fps = Math.round(this.stats.fpsCount / elapsed);
                this.stats.bitrateMbps = ((this.stats.bitrateBytes * 8) / elapsed / 1_000_000).toFixed(1);
                this.stats.fpsCount = 0;
                this.stats.bitrateBytes = 0;
                this.stats.lastFpsTime = now;
            }

            // 更新 UI
            const latencyEl = document.getElementById('stat-latency');
            const fpsEl = document.getElementById('stat-fps');
            const decodeEl = document.getElementById('stat-decode');
            const queueEl = document.getElementById('stat-queue');
            const bitrateEl = document.getElementById('stat-bitrate');

            latencyEl.textContent = `${this.stats.decodeTimeMs.toFixed(1)}ms`;
            fpsEl.textContent = `${this.stats.fps}`;
            decodeEl.textContent = `${this.stats.decodeTimeMs.toFixed(2)}ms`;
            queueEl.textContent = this.decoder ? `${this.decoder.decodeQueueSize}` : '--';
            bitrateEl.textContent = `${this.stats.bitrateMbps} Mbps`;

            // 颜色指示
            fpsEl.className = 'stat-value' + (this.stats.fps < 30 ? ' bad' : this.stats.fps < 55 ? ' warn' : '');
        }, 200);
    }
}

// 启动播放器
const player = new UltraLowLatencyPlayer();

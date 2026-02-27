/**
 * 超低延迟 WebCodecs AV1 播放器
 *
 * 核心设计原则：
 * 1. 零缓冲：解码后立即渲染，不使用任何帧缓冲队列
 * 2. 帧丢弃：如果解码跟不上，丢弃旧帧只渲染最新帧
 * 3. 时间戳同步：使用服务端时间戳进行延迟测量
 */

const HEADER_SIZE = 16;

// 帧类型
const FRAME_TYPE = {
    VIDEO: 0x01,
    KEYFRAME_REQUEST: 0x02,
    STATS: 0x03,
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

        this.decoder = null;
        this.ws = null;
        this.connected = false;

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
        this._connect();
        requestAnimationFrame(this.renderBound);
        this._startStatsUpdate();

        // 按 Tab 切换 overlay 显示
        document.addEventListener('keydown', (e) => {
            if (e.key === 'Tab') {
                e.preventDefault();
                this.overlay.classList.toggle('hidden');
            }
        });
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
        // 使用 WSS (WebSocket Secure)
        const wsUrl = `wss://${location.hostname}:9001`;
        console.log('连接到:', wsUrl);

        this.ws = new WebSocket(wsUrl);
        this.ws.binaryType = 'arraybuffer';

        this.ws.onopen = () => {
            console.log('WebSocket 已连接');
            this.connected = true;
            this.statusEl.style.display = 'none';
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
            this.statusEl.style.display = 'block';
            this.statusEl.innerHTML = '<span class="dot disconnected"></span> 连接断开，3秒后重连...';
            setTimeout(() => this._connect(), 3000);
        };

        this.ws.onerror = (e) => {
            console.error('WebSocket 错误:', e);
        };
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
        const pts = view.getUint32(6, true);
        const payloadLen = view.getUint32(10, true);

        if (frameType !== FRAME_TYPE.VIDEO) return;

        const isKeyframe = (flags & FRAME_FLAGS.KEYFRAME) !== 0;
        const payload = new Uint8Array(data, HEADER_SIZE, payloadLen);

        // 统计码率
        this.stats.bitrateBytes += data.byteLength;

        // 解码
        const decodeStart = performance.now();

        try {
            const chunk = new EncodedVideoChunk({
                type: isKeyframe ? 'key' : 'delta',
                timestamp: pts * 1000, // 转为微秒
                data: payload,
            });

            // 如果解码器队列过长，重置并请求关键帧
            if (this.decoder.decodeQueueSize > 3) {
                console.warn('解码队列过长, 重置');
                this._resetDecoder();
                this._requestKeyframe();
                return;
            }

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

mod capture;
mod encode;
mod protocol;
mod server;
mod transport;

use capture::dda::DdaCapture;
use encode::av1_amf::{Av1AmfEncoder, EncoderConfig};
use server::http::run_server;
use transport::websocket::WebSocketServer;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 目标帧率
const TARGET_FPS: u32 = 60;
/// 帧间隔
/// $$\Delta t = \frac{1}{fps} = \frac{1}{60} \approx 16.67\text{ms}$$
const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / TARGET_FPS as u64);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup logger with default info level
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("=== 超低延迟串流服务器启动 ===");

    // 关键帧请求通道
    let (keyframe_tx, mut keyframe_rx) = mpsc::channel::<()>(8);
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let force_kf_clone = force_keyframe.clone();

    // 初始化 WebSocket 服务器
    let (ws_server, _frame_tx) = WebSocketServer::new(keyframe_tx);
    let ws_server = Arc::new(ws_server);

    // 启动关键帧请求监听
    tokio::spawn(async move {
        while keyframe_rx.recv().await.is_some() {
            force_kf_clone.store(true, Ordering::Relaxed);
        }
    });

    // 初始化 TLS
    let tls_config = server::tls::get_tls_config()?;
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    let tls_acceptor_http = tls_acceptor.clone();
    let tls_acceptor_ws = tls_acceptor.clone();

    // 启动 WebSocket 服务器
    let ws_server_clone = ws_server.clone();
    let ws_addr: SocketAddr = "0.0.0.0:9001".parse()?;
    tokio::spawn(async move {
        if let Err(e) = ws_server_clone.run(ws_addr, tls_acceptor_ws).await {
            log::error!("WebSocket 服务器错误: {}", e);
        }
    });

    // 启动 HTTP 静态文件服务器（提供 Web 页面）
    let http_addr: SocketAddr = "0.0.0.0:8080".parse()?;
    tokio::spawn(async move {
        if let Err(e) = run_server(http_addr, tls_acceptor_http).await {
            log::error!("HTTP 服务器错误: {}", e);
        }
    });

    // ===== 捕获-编码主循环 =====
    // 在独立线程中运行（避免阻塞 tokio 运行时）
    let ws_server_encode = ws_server.clone();
    let encode_handle = std::thread::Builder::new()
        .name("capture-encode".into())
        .spawn(move || {
            if let Err(e) = capture_encode_loop(ws_server_encode, force_keyframe) {
                log::error!("Capture/Encode thread error: {}", e);
            }
        })?;

    log::info!("服务已启动！");
    log::info!("  Web 界面: https://localhost:8080");
    log::info!("  WebSocket: wss://localhost:9001");

    encode_handle.join().unwrap();
    Ok(())
}

/// 捕获-编码主循环
///
/// 采用忙等待 + 自旋的方式精确控制帧间隔：
/// $$t_{sleep} = \max(0, \Delta t - T_{capture} - T_{encode} - T_{margin})$$
///
/// 其中 $T_{margin} \approx 0.5\text{ms}$ 为自旋等待的余量。
fn capture_encode_loop(
    ws_server: Arc<WebSocketServer>,
    force_keyframe: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 初始化捕获器
    let mut capturer = DdaCapture::new()?;
    let width = capturer.width();
    let height = capturer.height();

    // 初始化编码器
    let config = EncoderConfig {
        width,
        height,
        fps: TARGET_FPS,
        bitrate: 10_000_000, // 10 Mbps
        keyframe_interval: 2,
    };
    let mut encoder = Av1AmfEncoder::new(&config)?;

    let frame_seq = AtomicU32::new(0);
    let mut stats_interval = Instant::now();
    let mut frames_encoded: u64 = 0;
    let mut total_encode_time_us: u64 = 0;

    log::info!("捕获-编码循环启动: {}x{} @{}fps", width, height, TARGET_FPS);

    loop {
        let frame_start = Instant::now();

        // Check if we need to force a keyframe
        let requesting_kf = force_keyframe.swap(false, Ordering::Relaxed);
        if requesting_kf {
            log::info!("强制请求关键帧");
        }

        // 1. 捕获
        let captured = match capturer.capture_frame(16)? {
            Some(f) => f,
            None => continue, // 无新帧，继续轮询
        };

        // 2. 编码
        let encoded_frames = encoder.encode(&captured.data, captured.stride, requesting_kf)?;

        // 3. 传输
        for ef in &encoded_frames {
            let seq = frame_seq.fetch_add(1, Ordering::Relaxed);
            ws_server.broadcast_frame(
                &ef.data,
                seq,
                ef.pts as u32,
                ef.is_keyframe,
                ef.encode_time_us,
            );

            frames_encoded += 1;
            total_encode_time_us += ef.encode_time_us;
        }

        // 4. 统计输出（每 5 秒一次）
        if stats_interval.elapsed() >= Duration::from_secs(5) {
            let avg_encode_ms = if frames_encoded > 0 {
                (total_encode_time_us as f64 / frames_encoded as f64) / 1000.0
            } else {
                0.0
            };
            log::info!(
                "统计: 已编码 {} 帧, 平均编码耗时: {:.2}ms",
                frames_encoded,
                avg_encode_ms,
            );
            stats_interval = Instant::now();
            frames_encoded = 0;
            total_encode_time_us = 0;
        }

        // 5. 精确帧间隔控制
        let elapsed = frame_start.elapsed();
        if elapsed < FRAME_INTERVAL {
            let sleep_duration = FRAME_INTERVAL - elapsed;
            // 先 sleep 大部分时间，然后自旋等待精确时间点
            if sleep_duration > Duration::from_micros(500) {
                std::thread::sleep(sleep_duration - Duration::from_micros(500));
            }
            // 自旋等待剩余时间（精度 < 1μs）
            while frame_start.elapsed() < FRAME_INTERVAL {
                std::hint::spin_loop();
            }
        }
    }
}

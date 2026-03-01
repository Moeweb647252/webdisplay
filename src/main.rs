mod capture;
mod encode;
mod input;
mod protocol;
mod server;
mod transport;

use capture::dda::DdaCapture;
use server::http::run_server;
use transport::webrtc::WebRtcServer;
use transport::websocket::WebSocketServer;
use transport::webtransport::WebTransportServer;

use std::net::SocketAddr;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup logger with default info level
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }

    log::info!("=== 串流服务器启动 ===");

    // 获取并序列化初始显示器列表
    let monitors = Arc::new(DdaCapture::enumerate_monitors().unwrap_or_default());
    for m in monitors.as_ref() {
        log::info!(
            "发现显示器 {}: {} ({}x{}){}",
            m.index,
            m.name,
            m.width,
            m.height,
            if m.primary { " [主屏]" } else { "" }
        );
    }
    let monitor_list_json = Arc::new(serde_json::to_vec(monitors.as_ref()).unwrap_or_default());

    // 初始化 WebSocket 服务器
    let ws_server = Arc::new(WebSocketServer::new(
        monitor_list_json.clone(),
        monitors.clone(),
    ));
    let wt_server = Arc::new(WebTransportServer::new(
        monitor_list_json.clone(),
        monitors.clone(),
    ));
    let webrtc_server = Arc::new(WebRtcServer::new(monitor_list_json, monitors));

    // 初始化 TLS
    let tls_config = server::tls::get_tls_config()?;
    let webtransport_cert_hash = Arc::new(server::tls::get_webtransport_certificate_hash_sha256()?);
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);

    // 启动统一 HTTP + WebSocket 服务器
    let server_addr: SocketAddr = "0.0.0.0:8080".parse()?;

    // 启动 WebTransport（UDP/QUIC）服务，与 HTTPS 共享端口号（不同协议）
    wt_server.clone().spawn(server_addr.port());

    log::info!("服务已启动！");
    log::info!("  Web 界面: https://localhost:8080");
    log::info!("  WebSocket: wss://localhost:8080/ws");
    log::info!("  WebTransport: https://localhost:8080/webtransport");
    log::info!("  WebRTC: https://localhost:8080/webrtc/offer");

    run_server(
        server_addr,
        tls_acceptor,
        ws_server,
        webrtc_server,
        webtransport_cert_hash,
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    Ok(())
}

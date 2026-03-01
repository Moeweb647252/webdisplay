use crate::transport::websocket::WebSocketServer;
use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::{get, get_service};
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

const CONTENT_SECURITY_POLICY: &str = "script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:; connect-src 'self' ws: wss: https:; style-src 'self' 'unsafe-inline';";
const ALT_SVC: &str = "h3=\":8080\"; ma=86400";

#[derive(Serialize)]
struct WebTransportHashResponse {
    algorithm: &'static str,
    value: Vec<u8>,
}

fn build_router(ws_server: Arc<WebSocketServer>, webtransport_cert_hash: Arc<Vec<u8>>) -> Router {
    let static_files =
        get_service(ServeDir::new("web/dist").append_index_html_on_directories(true));
    let hash_for_route = webtransport_cert_hash.clone();

    Router::new()
        .route("/ws", get(WebSocketServer::websocket_upgrade))
        .route(
            "/webtransport/hash",
            get(move || {
                let hash = hash_for_route.clone();
                async move {
                    Json(WebTransportHashResponse {
                        algorithm: "sha-256",
                        value: hash.as_ref().clone(),
                    })
                }
            }),
        )
        .with_state(ws_server)
        .fallback_service(static_files)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::ALT_SVC,
            HeaderValue::from_static(ALT_SVC),
        ))
}

pub async fn run_server(
    addr: SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    ws_server: Arc<WebSocketServer>,
    webtransport_cert_hash: Arc<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    let app = build_router(ws_server, webtransport_cert_hash);
    log::info!("HTTPS 服务器监听: https://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let app = app.clone();

        tokio::task::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    log::error!("TLS handshake error: {}", e);
                    return;
                }
            };

            let io = TokioIo::new(stream);
            let service = TowerToHyperService::new(app);

            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                log::debug!("HTTP server connection error: {}", err);
            }
        });
    }
}

use crate::transport::websocket::WebSocketServer;
use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, header};
use axum::routing::{get, get_service, post};
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
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

#[derive(Deserialize)]
struct WebRtcOfferRequest {
    sdp: String,
}

#[derive(Serialize)]
struct WebRtcAnswerResponse {
    sdp: String,
}

fn build_router(
    ws_server: Arc<WebSocketServer>,
    webrtc_server: Arc<crate::transport::webrtc::WebRtcServer>,
    webtransport_cert_hash: Arc<Vec<u8>>,
) -> Router {
    let static_files =
        get_service(ServeDir::new("web/dist").append_index_html_on_directories(true));
    let hash_for_route = webtransport_cert_hash.clone();

    // To cleanly share states and isolate them, we need to apply router combination strategies in Axum.
    // Instead of chained .with_state on the same router (which requires state types to match),
    // we use separate routers that are then `.merge()`d or nested.

    let webrtc_router = Router::new()
        .route(
            "/webrtc/offer",
            post(
                move |axum::extract::State(server): axum::extract::State<
                    Arc<crate::transport::webrtc::WebRtcServer>,
                >,
                      Json(payload): Json<WebRtcOfferRequest>| async move {
                    match server.handle_offer(payload.sdp).await {
                        Ok(sdp) => Ok(Json(WebRtcAnswerResponse { sdp })),
                        Err(e) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e)),
                    }
                },
            ),
        )
        .with_state(webrtc_server);

    let main_router = Router::new()
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
        .with_state(ws_server);

    main_router
        .merge(webrtc_router)
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
    webrtc_server: Arc<crate::transport::webrtc::WebRtcServer>,
    webtransport_cert_hash: Arc<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    let app = build_router(ws_server, webrtc_server, webtransport_cert_hash);
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

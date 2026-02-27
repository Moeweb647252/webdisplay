use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::Path;
use tokio::fs;
use tokio::net::TcpListener;

// Helper to serve files
async fn serve_file(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let mut path = req.uri().path();
    if path == "/" {
        path = "/index.html";
    }

    // Basic path traversal projection
    let sanitized_path = path.trim_start_matches('/');
    let target_path = Path::new("web").join(sanitized_path);

    // Basic safety check
    if !target_path.starts_with("web") && target_path.components().count() > 1 {
        return Ok(Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Full::new(Bytes::from("Forbidden")))
            .unwrap());
    }

    match fs::read(&target_path).await {
        Ok(content) => {
            let extension = target_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let content_type = match extension {
                "html" => "text/html",
                "js" => "application/javascript",
                "css" => "text/css",
                "wasm" => "application/wasm",
                _ => "application/octet-stream",
            };

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", content_type)
                .header(
                    "Content-Security-Policy",
                    "script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline';"
                )
                .body(Full::new(Bytes::from(content)))
                .unwrap())
        }
        Err(_) => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap()),
    }
}

pub async fn run_server(
    addr: SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    log::info!("HTTPS 服务器监听: https://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let acceptor = acceptor.clone();

        tokio::task::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    log::error!("TLS handshake error: {}", e);
                    return;
                }
            };

            let io = TokioIo::new(stream);

            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(serve_file))
                .await
            {
                log::debug!("HTTP server connection error: {}", err);
            }
        });
    }
}

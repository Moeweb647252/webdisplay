use super::session::{TransportIo, run_client_service};
use crate::capture::dda::MonitorInfo;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wtransport::endpoint::IncomingSession;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig};

/// WebTransport 读取块大小
const WT_READ_CHUNK_SIZE: usize = 64 * 1024;

/// WebTransport 单帧安全上限，避免恶意内存膨胀
const MAX_WT_FRAME_SIZE: usize = 64 * 1024 * 1024;

struct WebTransportIo {
    send_stream: SendStream,
    recv_stream: RecvStream,
    recv_buffer: Vec<u8>,
}

impl WebTransportIo {
    fn new(send_stream: SendStream, recv_stream: RecvStream) -> Self {
        Self {
            send_stream,
            recv_stream,
            recv_buffer: Vec::with_capacity(256 * 1024),
        }
    }

    fn try_take_packet_from_recv_buffer(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.recv_buffer.len() < 4 {
            return Ok(None);
        }

        let packet_len = u32::from_le_bytes([
            self.recv_buffer[0],
            self.recv_buffer[1],
            self.recv_buffer[2],
            self.recv_buffer[3],
        ]) as usize;

        if packet_len > MAX_WT_FRAME_SIZE {
            return Err(format!("WebTransport 包过大: {} bytes", packet_len));
        }

        let framed_len = 4usize
            .checked_add(packet_len)
            .ok_or_else(|| "WebTransport 包长度溢出".to_string())?;

        if self.recv_buffer.len() < framed_len {
            return Ok(None);
        }

        let packet = self.recv_buffer[4..framed_len].to_vec();
        self.recv_buffer.drain(..framed_len);
        Ok(Some(packet))
    }
}

impl TransportIo for WebTransportIo {
    fn send_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        let mut framed_packet = Vec::with_capacity(4 + packet.len());
        framed_packet.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        framed_packet.extend_from_slice(&packet);

        runtime.block_on(async {
            self.send_stream
                .write_all(&framed_packet)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn recv_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        if let Some(packet) = self.try_take_packet_from_recv_buffer()? {
            return Ok(Some(packet));
        }

        let loop_start = Instant::now();
        let mut scratch = [0u8; WT_READ_CHUNK_SIZE];

        loop {
            let wait = if timeout == Duration::ZERO {
                Duration::ZERO
            } else {
                let elapsed = loop_start.elapsed();
                if elapsed >= timeout {
                    return Ok(None);
                }
                timeout - elapsed
            };

            let read_res = runtime.block_on(async {
                tokio::time::timeout(wait, self.recv_stream.read(&mut scratch)).await
            });

            let bytes_read = match read_res {
                Ok(inner) => inner.map_err(|e| e.to_string())?,
                Err(_) => return Ok(None),
            };

            let Some(bytes_read) = bytes_read else {
                return Err("WebTransport 连接已关闭".to_string());
            };

            if bytes_read == 0 {
                continue;
            }

            self.recv_buffer.extend_from_slice(&scratch[..bytes_read]);

            if let Some(packet) = self.try_take_packet_from_recv_buffer()? {
                return Ok(Some(packet));
            }
        }
    }
}

/// WebTransport 串流服务器（QUIC/HTTP3）
pub struct WebTransportServer {
    /// 缓存的显示器列表 JSON 数据
    monitor_list_json: Arc<Vec<u8>>,
    /// 显示器元数据（用于输入坐标映射）
    monitors: Arc<Vec<MonitorInfo>>,
}

impl WebTransportServer {
    pub fn new(monitor_list_json: Arc<Vec<u8>>, monitors: Arc<Vec<MonitorInfo>>) -> Self {
        Self {
            monitor_list_json,
            monitors,
        }
    }

    pub fn spawn(self: Arc<Self>, port: u16) {
        tokio::spawn(async move {
            if let Err(e) = self.run(port).await {
                log::warn!("WebTransport 服务不可用，将仅使用 WebSocket: {}", e);
            }
        });
    }

    async fn run(self: Arc<Self>, port: u16) -> Result<(), String> {
        let identity = Identity::load_pemfiles("cert.pem", "key.pem")
            .await
            .map_err(|e| format!("加载 WebTransport TLS 证书失败: {}", e))?;

        let config = ServerConfig::builder()
            .with_bind_default(port)
            .with_identity(identity)
            .keep_alive_interval(Some(Duration::from_secs(3)))
            .build();

        let endpoint = Endpoint::server(config).map_err(|e| e.to_string())?;
        log::info!(
            "WebTransport 服务器监听: https://localhost:{}/webtransport (UDP/QUIC)",
            port
        );

        loop {
            let incoming_session = endpoint.accept().await;
            let server = Arc::clone(&self);

            tokio::spawn(async move {
                if let Err(e) = server.handle_incoming_session(incoming_session).await {
                    log::warn!("WebTransport 客户端断开: {}", e);
                }
            });
        }
    }

    async fn handle_incoming_session(
        self: Arc<Self>,
        incoming_session: IncomingSession,
    ) -> Result<(), String> {
        let session_request = incoming_session.await.map_err(|e| e.to_string())?;
        let authority = session_request.authority().to_owned();
        let path = session_request.path().to_owned();

        if path != "/" && path != "/webtransport" && !path.starts_with("/webtransport/") {
            return Err(format!("不支持的 WebTransport 路径: {}", path));
        }

        let connection = session_request.accept().await.map_err(|e| e.to_string())?;
        log::info!(
            "WebTransport 会话已建立: authority='{}', path='{}'",
            authority,
            path
        );

        self.handle_client(connection).await
    }

    /// 为单个客户端启动独立服务（捕获 + 编码 + 发送 + 控制）
    async fn handle_client(&self, connection: Connection) -> Result<(), String> {
        let monitor_list_json = self.monitor_list_json.clone();
        let monitors = self.monitors.clone();
        let runtime = tokio::runtime::Handle::current();

        let (send_stream, recv_stream) = connection
            .accept_bi()
            .await
            .map_err(|e| format!("等待 WebTransport 双向流失败: {}", e))?;

        let io = WebTransportIo::new(send_stream, recv_stream);

        let task = tokio::task::spawn_blocking(move || {
            run_client_service(runtime, io, monitor_list_json, monitors, "WebTransport")
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(format!("WebTransport 客户端服务线程异常: {}", e)),
        }
    }
}

use super::session::{TransportIo, run_client_service};
use crate::capture::dda::MonitorInfo;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// WebSocket 串流服务器
pub struct WebSocketServer {
    /// 缓存的显示器列表 JSON 数据
    monitor_list_json: Arc<Vec<u8>>,
    /// 显示器元数据（用于输入坐标映射）
    monitors: Arc<Vec<MonitorInfo>>,
}

impl WebSocketServer {
    pub fn new(monitor_list_json: Arc<Vec<u8>>, monitors: Arc<Vec<MonitorInfo>>) -> Self {
        Self {
            monitor_list_json,
            monitors,
        }
    }

    pub async fn websocket_upgrade(
        State(server): State<Arc<WebSocketServer>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| async move {
            if let Err(e) = server.handle_client(socket).await {
                log::warn!("WebSocket 客户端断开: {}", e);
            }
        })
    }

    /// 为单个客户端启动独立服务（捕获 + 编码 + 发送 + 控制）
    async fn handle_client(&self, socket: WebSocket) -> Result<(), String> {
        let monitor_list_json = self.monitor_list_json.clone();
        let monitors = self.monitors.clone();
        let runtime = tokio::runtime::Handle::current();
        let io = WebSocketIo::new(socket);

        let task = tokio::task::spawn_blocking(move || {
            run_client_service(runtime, io, monitor_list_json, monitors, "WebSocket")
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(format!("WebSocket 客户端服务线程异常: {}", e)),
        }
    }
}

struct WebSocketIo {
    socket: WebSocket,
}

impl WebSocketIo {
    fn new(socket: WebSocket) -> Self {
        Self { socket }
    }
}

impl TransportIo for WebSocketIo {
    fn send_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        runtime
            .block_on(self.socket.send(Message::Binary(packet.into())))
            .map_err(|e| e.to_string())
    }

    fn recv_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        loop {
            let next_msg = match runtime.block_on(tokio::time::timeout(timeout, self.socket.next()))
            {
                Ok(msg) => msg,
                Err(_) => return Ok(None),
            };

            match next_msg {
                Some(Ok(Message::Binary(data))) => return Ok(Some(data.to_vec())),
                Some(Ok(Message::Ping(payload))) => {
                    runtime
                        .block_on(self.socket.send(Message::Pong(payload)))
                        .map_err(|e| e.to_string())?;
                }
                Some(Ok(Message::Close(_))) => return Err("WebSocket 已关闭".to_string()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("接收客户端消息失败: {}", e)),
                None => return Err("WebSocket 已关闭".to_string()),
            }
        }
    }
}

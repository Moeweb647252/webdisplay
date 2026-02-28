use crate::protocol::frame::{FrameFlags, FrameHeader, FrameType};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;

/// WebSocket 串流服务器
///
/// 最大传输单元考虑：
/// 对于单帧 AV1 编码数据，在 10Mbps@60fps 下，
/// 平均每帧大小约为:
/// $$S_{frame} = \frac{B}{fps} = \frac{10 \times 10^6}{60} \approx 20.8 \text{KB}$$
///
/// WebSocket 单帧可承载此大小，无需分片。
pub struct WebSocketServer {
    /// 帧广播通道（支持多客户端）
    frame_tx: broadcast::Sender<Arc<Vec<u8>>>,
    /// 关键帧请求回调
    keyframe_request_tx: tokio::sync::mpsc::Sender<()>,
    /// 显示器切换请求回调
    monitor_switch_tx: tokio::sync::mpsc::Sender<u32>,
    /// 缓存的显示器列表 JSON 数据
    monitor_list_json: Arc<Vec<u8>>,
}

impl WebSocketServer {
    pub fn new(
        keyframe_request_tx: tokio::sync::mpsc::Sender<()>,
        monitor_switch_tx: tokio::sync::mpsc::Sender<u32>,
        monitor_list_json: Arc<Vec<u8>>,
    ) -> (Self, broadcast::Sender<Arc<Vec<u8>>>) {
        // 缓冲区大小：保留最近 4 帧，旧帧直接丢弃
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(4);
        let tx_clone = frame_tx.clone();

        (
            Self {
                frame_tx,
                keyframe_request_tx,
                monitor_switch_tx,
                monitor_list_json,
            },
            tx_clone,
        )
    }

    pub async fn websocket_upgrade(
        State(server): State<Arc<WebSocketServer>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| async move {
            if let Err(e) = server.handle_client(socket).await {
                log::warn!("客户端断开: {}", e);
            }
        })
    }

    /// 处理单个客户端连接
    async fn handle_client(
        &self,
        socket: WebSocket,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut frame_rx = self.frame_tx.subscribe();
        let keyframe_tx = self.keyframe_request_tx.clone();
        let monitor_switch_tx = self.monitor_switch_tx.clone();
        let monitor_list_json = self.monitor_list_json.clone();
        let (mut ws_sender, mut ws_receiver) = socket.split();

        // 建立连接后立即发送显示器列表
        let header = FrameHeader {
            frame_type: FrameType::MonitorList,
            flags: FrameFlags::empty(),
            sequence: 0,
            pts: 0,
            payload_len: monitor_list_json.len() as u32,
        };
        let mut packet = Vec::with_capacity(FrameHeader::SIZE + monitor_list_json.len());
        packet.extend_from_slice(&header.to_bytes());
        packet.extend_from_slice(&monitor_list_json);
        if let Err(e) = ws_sender.send(Message::Binary(packet.into())).await {
            log::warn!("发送初始显示器列表失败: {}", e);
            return Ok(());
        }

        // 发送任务：将编码帧推送给客户端
        let keyframe_tx1 = keyframe_tx.clone();
        let send_task = tokio::spawn(async move {
            loop {
                match frame_rx.recv().await {
                    Ok(frame_data) => {
                        if ws_sender
                            .send(Message::Binary(frame_data.as_ref().clone().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 客户端落后，跳过旧帧
                        log::warn!("客户端落后 {} 帧，跳过", n);
                        // 请求关键帧以便重新同步
                        let _ = keyframe_tx1.send(()).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // 接收任务：处理客户端消息（关键帧请求等）
        let keyframe_tx2 = keyframe_tx;
        let recv_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = ws_receiver.next().await {
                if let Message::Binary(data) = msg {
                    if data.len() >= FrameHeader::SIZE {
                        let Ok(header_bytes) = data[..FrameHeader::SIZE].try_into() else {
                            continue;
                        };
                        if let Some(header) = FrameHeader::from_bytes(&header_bytes) {
                            if header.frame_type == FrameType::KeyframeRequest {
                                log::info!("客户端请求关键帧");
                                let _ = keyframe_tx2.send(()).await;
                            } else if header.frame_type == FrameType::MonitorSelect {
                                let start = FrameHeader::SIZE;
                                let end = start + header.payload_len as usize;
                                if data.len() >= end {
                                    if let Ok(json) = std::str::from_utf8(&data[start..end]) {
                                        if let Ok(val) =
                                            serde_json::from_str::<serde_json::Value>(json)
                                        {
                                            if let Some(index) =
                                                val.get("index").and_then(|v| v.as_u64())
                                            {
                                                log::info!("客户端请求切换屏幕到 {}", index);
                                                let _ = monitor_switch_tx.send(index as u32).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        tokio::select! {
            _ = send_task => {}
            _ = recv_task => {}
        }

        log::info!("客户端已断开");
        Ok(())
    }

    /// 打包并广播一个编码帧
    pub fn broadcast_frame(
        &self,
        encoded_data: &[u8],
        sequence: u32,
        pts: u32,
        is_keyframe: bool,
        _encode_time_us: u64,
    ) {
        let mut flags = FrameFlags::END_OF_FRAME;
        if is_keyframe {
            flags |= FrameFlags::KEYFRAME;
        }

        let header = FrameHeader {
            frame_type: FrameType::VideoFrame,
            flags,
            sequence,
            pts,
            payload_len: encoded_data.len() as u32,
        };

        let mut packet = Vec::with_capacity(FrameHeader::SIZE + encoded_data.len());
        packet.extend_from_slice(&header.to_bytes());
        packet.extend_from_slice(encoded_data);

        // 广播到所有客户端，忽略无接收者的错误
        let _ = self.frame_tx.send(Arc::new(packet));
    }
}

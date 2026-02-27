use crate::protocol::frame::{FrameFlags, FrameHeader, FrameType};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

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
}

impl WebSocketServer {
    pub fn new(
        keyframe_request_tx: tokio::sync::mpsc::Sender<()>,
    ) -> (Self, broadcast::Sender<Arc<Vec<u8>>>) {
        // 缓冲区大小：保留最近 4 帧，旧帧直接丢弃
        let (frame_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(4);
        let tx_clone = frame_tx.clone();

        (
            Self {
                frame_tx,
                keyframe_request_tx,
            },
            tx_clone,
        )
    }

    /// 启动 WebSocket 服务器
    pub async fn run(
        &self,
        addr: SocketAddr,
        acceptor: tokio_rustls::TlsAcceptor,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        log::info!("WebSocket 服务器监听: wss://{}", addr);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let acceptor = acceptor.clone();
            log::info!("新客户端连接尝试: {}", peer_addr);

            let frame_rx = self.frame_tx.subscribe();
            let keyframe_tx = self.keyframe_request_tx.clone();

            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        log::error!("WS TLS handshake error from {}: {}", peer_addr, e);
                        return;
                    }
                };
                if let Err(e) = Self::handle_client(stream, frame_rx, keyframe_tx, peer_addr).await
                {
                    log::warn!("客户端 {} 断开: {}", peer_addr, e);
                }
            });
        }
    }

    /// 处理单个客户端连接
    async fn handle_client(
        stream: tokio_rustls::server::TlsStream<TcpStream>,
        mut frame_rx: broadcast::Receiver<Arc<Vec<u8>>>,
        keyframe_tx: tokio::sync::mpsc::Sender<()>,
        peer_addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 禁用 Nagle 算法已经在 acceptor.accept 前处理

        let ws_stream = tokio_tungstenite::accept_async(stream).await?;
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // 发送任务：将编码帧推送给客户端
        let keyframe_tx1 = keyframe_tx.clone();
        let send_task = tokio::spawn(async move {
            let mut _last_keyframe_data: Option<Arc<Vec<u8>>> = None;

            loop {
                match frame_rx.recv().await {
                    Ok(frame_data) => {
                        // 检查是否是关键帧（检查 header 标志位）
                        if frame_data.len() >= FrameHeader::SIZE {
                            let flags = FrameFlags::from_bits_truncate(frame_data[1]);
                            if flags.contains(FrameFlags::KEYFRAME) {
                                _last_keyframe_data = Some(frame_data.clone());
                            }
                        }

                        if ws_sender
                            .send(Message::Binary(frame_data.to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 客户端落后，跳过旧帧
                        log::warn!("客户端 {} 落后 {} 帧，跳过", peer_addr, n);
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
                        let header_bytes: [u8; FrameHeader::SIZE] =
                            data[..FrameHeader::SIZE].try_into().unwrap();
                        if let Some(header) = FrameHeader::from_bytes(&header_bytes) {
                            if header.frame_type == FrameType::KeyframeRequest {
                                log::info!("客户端 {} 请求关键帧", peer_addr);
                                let _ = keyframe_tx2.send(()).await;
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

        log::info!("客户端 {} 已断开", peer_addr);
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

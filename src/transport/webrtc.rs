use crate::capture::dda::MonitorInfo;
use crate::transport::session::{TransportIo, run_client_service};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

struct WebRtcIo {
    sender: mpsc::Sender<Vec<u8>>,
    receiver: mpsc::Receiver<Vec<u8>>,
}

impl TransportIo for WebRtcIo {
    fn send_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        let sender = self.sender.clone();
        runtime.block_on(async { sender.send(packet).await.map_err(|e| e.to_string()) })
    }

    fn recv_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        if timeout == Duration::ZERO {
            match self.receiver.try_recv() {
                Ok(packet) => Ok(Some(packet)),
                Err(mpsc::error::TryRecvError::Empty) => Ok(None),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    Err("WebRTC 连接已关闭".to_string())
                }
            }
        } else {
            runtime.block_on(async {
                match tokio::time::timeout(timeout, self.receiver.recv()).await {
                    Ok(Some(packet)) => Ok(Some(packet)),
                    Ok(None) => Err("WebRTC 连接已关闭".to_string()),
                    Err(_) => Ok(None), // timeout
                }
            })
        }
    }
}

pub struct WebRtcServer {
    monitor_list_json: Arc<Vec<u8>>,
    monitors: Arc<Vec<MonitorInfo>>,
}

impl WebRtcServer {
    pub fn new(monitor_list_json: Arc<Vec<u8>>, monitors: Arc<Vec<MonitorInfo>>) -> Self {
        Self {
            monitor_list_json,
            monitors,
        }
    }

    pub async fn handle_offer(&self, offer_string: String) -> Result<String, String> {
        let api = APIBuilder::new().build();
        let config = RTCConfiguration::default();
        let peer_connection = Arc::new(
            api.new_peer_connection(config)
                .await
                .map_err(|e| e.to_string())?,
        );

        // peer_connection is an Arc<RTCPeerConnection> from api.new_peer_connection
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                log::info!("WebRTC连接状态变为: {}", s);
                if s == RTCPeerConnectionState::Failed || s == RTCPeerConnectionState::Closed {
                    log::info!("WebRTC已断开，清理资源");
                    // The PC drop/close is handled automatically or by closing from the client
                }
                Box::pin(async {})
            },
        ));

        let monitor_list_json = self.monitor_list_json.clone();
        let monitors = self.monitors.clone();
        let runtime = tokio::runtime::Handle::current();

        peer_connection.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            log::info!("WebRTC DataChannel 已捕获: {}", d.label());
            let d_clone = Arc::clone(&d);

            let monitor_list_json = monitor_list_json.clone();
            let monitors = monitors.clone();
            let runtime = runtime.clone();

            Box::pin(async move {
                let (io_tx, io_rx) = mpsc::channel(256); // from client to server (received events)
                let (srv_tx, mut srv_rx) = mpsc::channel::<Vec<u8>>(256); // from server to client (send packets)

                d_clone.on_message(Box::new(move |msg| {
                    let io_tx = io_tx.clone();
                    Box::pin(async move {
                        let _ = io_tx.send(msg.data.to_vec()).await;
                    })
                }));

                let d_sender = Arc::clone(&d_clone);
                tokio::spawn(async move {
                    // webrtc data channel default max message size is 65535, we use 60000 to be safe
                    const MAX_CHUNK_SIZE: usize = 60000;

                    while let Some(packet) = srv_rx.recv().await {
                        let total_len = packet.len();

                        // We will prepend a 4 byte header indicating the size of the whole packet
                        // so that the client can re-assemble chunks.
                        // Format: [4 byte total length][4 byte offset][chunk data]

                        let mut offset = 0;
                        while offset < total_len {
                            let chunk_size = std::cmp::min(MAX_CHUNK_SIZE, total_len - offset);
                            let mut chunk = Vec::with_capacity(chunk_size + 8);
                            chunk.extend_from_slice(&(total_len as u32).to_le_bytes());
                            chunk.extend_from_slice(&(offset as u32).to_le_bytes());
                            chunk.extend_from_slice(&packet[offset..offset + chunk_size]);

                            if let Err(e) = d_sender.send(&bytes::Bytes::from(chunk)).await {
                                log::warn!("WebRTC data channel 发送失败: {}", e);
                                return; // Stop on error
                            }

                            offset += chunk_size;
                        }
                    }
                });

                d_clone.on_open(Box::new(move || {
                    log::info!("WebRTC DataChannel 已打开，开始服务");
                    let io = WebRtcIo {
                        sender: srv_tx.clone(),
                        receiver: io_rx,
                    };

                    let rt = runtime.clone();
                    let ml = monitor_list_json.clone();
                    let m = monitors.clone();

                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = run_client_service(rt, io, ml, m, "WebRTC") {
                            log::warn!("WebRTC 客户端服务线程异常: {}", e);
                        }
                    });

                    Box::pin(async {})
                }));
            })
        }));

        let offer = RTCSessionDescription::offer(offer_string).map_err(|e| e.to_string())?;
        peer_connection
            .set_remote_description(offer)
            .await
            .map_err(|e| e.to_string())?;

        let answer = peer_connection
            .create_answer(None)
            .await
            .map_err(|e| e.to_string())?;
        peer_connection
            .set_local_description(answer.clone())
            .await
            .map_err(|e| e.to_string())?;

        Ok(answer.sdp)
    }
}

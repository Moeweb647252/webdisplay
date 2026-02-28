use crate::capture::dda::DdaCapture;
use crate::encode::av1_amf::{Av1AmfEncoder, EncoderConfig};
use crate::protocol::frame::{FrameFlags, FrameHeader, FrameType};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 目标帧率
const TARGET_FPS: u32 = 60;
/// 帧间隔
/// $$\Delta t = \frac{1}{fps} = \frac{1}{60} \approx 16.67\text{ms}$$
const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / TARGET_FPS as u64);
/// 目标码率 (bps)
const TARGET_BITRATE: usize = 10_000_000;
/// 关键帧间隔（秒）
const KEYFRAME_INTERVAL_SECS: u32 = 2;
/// 控制消息轮询超时
const CONTROL_POLL_TIMEOUT: Duration = Duration::from_millis(1);

enum ClientConnectionState {
    Alive,
    Closed,
}

/// WebSocket 串流服务器
///
/// 最大传输单元考虑：
/// 对于单帧 AV1 编码数据，在 10Mbps@60fps 下，
/// 平均每帧大小约为:
/// $$S_{frame} = \frac{B}{fps} = \frac{10 \times 10^6}{60} \approx 20.8 \text{KB}$$
///
/// WebSocket 单帧可承载此大小，无需分片。
pub struct WebSocketServer {
    /// 缓存的显示器列表 JSON 数据
    monitor_list_json: Arc<Vec<u8>>,
}

impl WebSocketServer {
    pub fn new(monitor_list_json: Arc<Vec<u8>>) -> Self {
        Self { monitor_list_json }
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

    /// 为单个客户端启动独立服务（捕获 + 编码 + 发送 + 控制）
    async fn handle_client(&self, socket: WebSocket) -> Result<(), String> {
        let monitor_list_json = self.monitor_list_json.clone();
        let runtime = tokio::runtime::Handle::current();

        let task = tokio::task::spawn_blocking(move || {
            Self::run_client_service(runtime, socket, monitor_list_json)
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(format!("客户端服务线程异常: {}", e)),
        }
    }

    fn run_client_service(
        runtime: tokio::runtime::Handle,
        mut socket: WebSocket,
        monitor_list_json: Arc<Vec<u8>>,
    ) -> Result<(), String> {
        // 建立连接后立即发送显示器列表
        Self::send_monitor_list(&runtime, &mut socket, monitor_list_json.as_ref())?;

        let mut current_monitor_index = 0;
        let mut capturer = DdaCapture::new(current_monitor_index).map_err(|e| e.to_string())?;
        let mut encoder =
            Av1AmfEncoder::new(&Self::encoder_config(capturer.width(), capturer.height()))
                .map_err(|e| e.to_string())?;

        let mut force_keyframe = true;
        let mut pending_monitor_switch = None::<u32>;
        let mut frame_seq = 0u32;

        let mut stats_interval = Instant::now();
        let mut frames_encoded: u64 = 0;
        let mut total_encode_time_us: u64 = 0;

        log::info!(
            "客户端独立服务启动: monitor {}, {}x{} @{}fps",
            current_monitor_index,
            capturer.width(),
            capturer.height(),
            TARGET_FPS
        );

        loop {
            match Self::drain_control_messages(
                &runtime,
                &mut socket,
                &mut force_keyframe,
                &mut pending_monitor_switch,
            )? {
                ClientConnectionState::Alive => {}
                ClientConnectionState::Closed => {
                    log::info!("客户端已断开");
                    return Ok(());
                }
            }

            if let Some(new_index) = pending_monitor_switch.take() {
                if Self::switch_monitor(
                    new_index,
                    &mut current_monitor_index,
                    &mut capturer,
                    &mut encoder,
                )? {
                    force_keyframe = true;
                }
            }

            let frame_start = Instant::now();

            let requesting_kf = std::mem::take(&mut force_keyframe);
            if requesting_kf {
                log::info!("客户端请求关键帧");
            }

            let captured = match capturer.capture_frame(16).map_err(|e| e.to_string())? {
                Some(frame) => frame,
                None => {
                    Self::pace_frame(frame_start);
                    continue;
                }
            };

            let encoded_frames = encoder
                .encode(&captured.data, captured.stride, requesting_kf)
                .map_err(|e| e.to_string())?;

            for ef in encoded_frames {
                let packet =
                    Self::build_video_packet(&ef.data, frame_seq, ef.pts as u32, ef.is_keyframe);
                frame_seq = frame_seq.wrapping_add(1);

                if Self::send_binary_packet(&runtime, &mut socket, packet).is_err() {
                    log::info!("客户端已断开");
                    return Ok(());
                }

                frames_encoded += 1;
                total_encode_time_us += ef.encode_time_us;
            }

            if stats_interval.elapsed() >= Duration::from_secs(5) {
                let avg_encode_ms = if frames_encoded > 0 {
                    (total_encode_time_us as f64 / frames_encoded as f64) / 1000.0
                } else {
                    0.0
                };
                log::info!(
                    "客户端统计: 已编码 {} 帧, 平均编码耗时: {:.2}ms",
                    frames_encoded,
                    avg_encode_ms,
                );
                stats_interval = Instant::now();
                frames_encoded = 0;
                total_encode_time_us = 0;
            }

            Self::pace_frame(frame_start);
        }
    }

    fn send_monitor_list(
        runtime: &tokio::runtime::Handle,
        socket: &mut WebSocket,
        monitor_list_json: &[u8],
    ) -> Result<(), String> {
        let header = FrameHeader {
            frame_type: FrameType::MonitorList,
            flags: FrameFlags::empty(),
            sequence: 0,
            pts: 0,
            payload_len: monitor_list_json.len() as u32,
        };
        let mut packet = Vec::with_capacity(FrameHeader::SIZE + monitor_list_json.len());
        packet.extend_from_slice(&header.to_bytes());
        packet.extend_from_slice(monitor_list_json);

        if let Err(e) = Self::send_binary_packet(runtime, socket, packet) {
            log::warn!("发送初始显示器列表失败: {}", e);
            return Err(e);
        }
        Ok(())
    }

    fn send_binary_packet(
        runtime: &tokio::runtime::Handle,
        socket: &mut WebSocket,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        runtime
            .block_on(socket.send(Message::Binary(packet.into())))
            .map_err(|e| e.to_string())
    }

    fn drain_control_messages(
        runtime: &tokio::runtime::Handle,
        socket: &mut WebSocket,
        force_keyframe: &mut bool,
        pending_monitor_switch: &mut Option<u32>,
    ) -> Result<ClientConnectionState, String> {
        loop {
            let next_msg =
                match runtime.block_on(tokio::time::timeout(CONTROL_POLL_TIMEOUT, socket.next())) {
                    Ok(msg) => msg,
                    Err(_) => return Ok(ClientConnectionState::Alive),
                };

            match next_msg {
                Some(Ok(Message::Binary(data))) => {
                    Self::handle_binary_control_message(
                        &data,
                        force_keyframe,
                        pending_monitor_switch,
                    );
                }
                Some(Ok(Message::Ping(payload))) => {
                    if runtime
                        .block_on(socket.send(Message::Pong(payload)))
                        .is_err()
                    {
                        return Ok(ClientConnectionState::Closed);
                    }
                }
                Some(Ok(Message::Close(_))) => return Ok(ClientConnectionState::Closed),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    log::warn!("接收客户端消息失败: {}", e);
                    return Ok(ClientConnectionState::Closed);
                }
                None => return Ok(ClientConnectionState::Closed),
            }
        }
    }

    fn handle_binary_control_message(
        data: &[u8],
        force_keyframe: &mut bool,
        pending_monitor_switch: &mut Option<u32>,
    ) {
        if data.len() < FrameHeader::SIZE {
            return;
        }

        let Ok(header_bytes) = data[..FrameHeader::SIZE].try_into() else {
            return;
        };
        let Some(header) = FrameHeader::from_bytes(&header_bytes) else {
            return;
        };

        match header.frame_type {
            FrameType::KeyframeRequest => {
                *force_keyframe = true;
            }
            FrameType::MonitorSelect => {
                if let Some(index) = Self::parse_monitor_index(data, header.payload_len) {
                    *pending_monitor_switch = Some(index);
                }
            }
            _ => {}
        }
    }

    fn parse_monitor_index(data: &[u8], payload_len: u32) -> Option<u32> {
        let start = FrameHeader::SIZE;
        let end = start.checked_add(payload_len as usize)?;
        if end > data.len() {
            return None;
        }

        let json = std::str::from_utf8(&data[start..end]).ok()?;
        let val = serde_json::from_str::<serde_json::Value>(json).ok()?;
        val.get("index").and_then(|v| v.as_u64()).map(|v| v as u32)
    }

    fn switch_monitor(
        new_index: u32,
        current_monitor_index: &mut u32,
        capturer: &mut DdaCapture,
        encoder: &mut Av1AmfEncoder,
    ) -> Result<bool, String> {
        if new_index == *current_monitor_index {
            return Ok(false);
        }

        log::info!("客户端请求切换屏幕到 {}", new_index);
        let new_capturer = match DdaCapture::new(new_index) {
            Ok(c) => c,
            Err(e) => {
                log::error!("切换显示器失败: {}", e);
                return Ok(false);
            }
        };

        let new_encoder = match Av1AmfEncoder::new(&Self::encoder_config(
            new_capturer.width(),
            new_capturer.height(),
        )) {
            Ok(e) => e,
            Err(e) => {
                log::error!("切换显示器后初始化编码器失败: {}", e);
                return Ok(false);
            }
        };

        *capturer = new_capturer;
        *encoder = new_encoder;
        *current_monitor_index = new_index;

        log::info!("显示器切换成功：{}x{}", capturer.width(), capturer.height());
        Ok(true)
    }

    fn encoder_config(width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            fps: TARGET_FPS,
            bitrate: TARGET_BITRATE,
            keyframe_interval: KEYFRAME_INTERVAL_SECS,
        }
    }

    fn build_video_packet(
        encoded_data: &[u8],
        sequence: u32,
        pts: u32,
        is_keyframe: bool,
    ) -> Vec<u8> {
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
        packet
    }

    fn pace_frame(frame_start: Instant) {
        let elapsed = frame_start.elapsed();
        if elapsed < FRAME_INTERVAL {
            let sleep_duration = FRAME_INTERVAL - elapsed;
            if sleep_duration > Duration::from_micros(500) {
                std::thread::sleep(sleep_duration - Duration::from_micros(500));
            }
            while frame_start.elapsed() < FRAME_INTERVAL {
                std::hint::spin_loop();
            }
        }
    }
}

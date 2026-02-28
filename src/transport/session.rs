use crate::capture::dda::{DdaCapture, MonitorInfo};
use crate::encode::amf::{AmfEncoder, EncoderConfig, VideoCodec};
use crate::input::win32::{ActiveMonitor, InputInjector};
use crate::protocol::frame::{FrameFlags, FrameHeader, FrameType};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 默认目标帧率
const DEFAULT_TARGET_FPS: u32 = 60;
const MIN_TARGET_FPS: u32 = 24;
const MAX_TARGET_FPS: u32 = 120;

/// 默认目标码率 (bps)
const DEFAULT_TARGET_BITRATE: usize = 20_000_000;
const MIN_TARGET_BITRATE: usize = 2_000_000;
const MAX_TARGET_BITRATE: usize = 80_000_000;

/// 默认关键帧间隔（秒）
const DEFAULT_KEYFRAME_INTERVAL_SECS: u32 = 2;
const MIN_KEYFRAME_INTERVAL_SECS: u32 = 1;
const MAX_KEYFRAME_INTERVAL_SECS: u32 = 10;

/// 控制消息轮询超时（使用零超时避免浪费帧时间预算）
const CONTROL_POLL_TIMEOUT: Duration = Duration::ZERO;

#[derive(Debug, Clone, Copy)]
struct EncodingSettings {
    codec: VideoCodec,
    fps: u32,
    bitrate: usize,
    keyframe_interval_secs: u32,
}

impl Default for EncodingSettings {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Av1,
            fps: DEFAULT_TARGET_FPS,
            bitrate: DEFAULT_TARGET_BITRATE,
            keyframe_interval_secs: DEFAULT_KEYFRAME_INTERVAL_SECS,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EncodingSettingsPayload {
    fps: u32,
    bitrate: u32,
    keyframe_interval: u32,
    #[serde(default)]
    codec: Option<String>,
}

#[derive(Debug, Serialize)]
struct EncodingSettingsStatePayload {
    fps: u32,
    bitrate: u32,
    keyframe_interval: u32,
    codec: &'static str,
}

enum ClientConnectionState {
    Alive,
    Closed,
}

#[derive(Debug, Deserialize)]
struct MonitorSelectPayload {
    index: u32,
}

#[derive(Debug, Deserialize)]
struct KeyboardInputPayload {
    key_code: u16,
    down: bool,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MouseInputPayload {
    Move {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: u8,
        down: bool,
    },
    Wheel {
        x: f32,
        y: f32,
        delta_x: i32,
        delta_y: i32,
    },
}

pub(crate) trait TransportIo {
    fn send_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        packet: Vec<u8>,
    ) -> Result<(), String>;

    fn recv_packet(
        &mut self,
        runtime: &tokio::runtime::Handle,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String>;
}

pub(crate) fn run_client_service<T: TransportIo>(
    runtime: tokio::runtime::Handle,
    mut io: T,
    monitor_list_json: Arc<Vec<u8>>,
    monitors: Arc<Vec<MonitorInfo>>,
    transport_name: &'static str,
) -> Result<(), String> {
    // 建立连接后立即发送显示器列表
    send_monitor_list(&runtime, &mut io, monitor_list_json.as_ref())?;

    let mut encoding_settings = EncodingSettings::default();
    let mut frame_interval = frame_interval_for_fps(encoding_settings.fps);
    let mut capture_timeout_ms = capture_timeout_ms_for_fps(encoding_settings.fps);

    let mut current_monitor_index = 0;
    let mut capturer = DdaCapture::new(current_monitor_index).map_err(|e| e.to_string())?;
    let mut encoder = AmfEncoder::new(&encoder_config(
        capturer.width(),
        capturer.height(),
        encoding_settings,
    ))
    .map_err(|e| e.to_string())?;
    let mut active_monitor = resolve_active_monitor(
        monitors.as_ref(),
        current_monitor_index,
        capturer.width(),
        capturer.height(),
    );

    let input_injector = match InputInjector::new() {
        Ok(injector) => Some(injector),
        Err(e) => {
            log::warn!("初始化输入注入失败，将禁用远程输入: {}", e);
            None
        }
    };

    let mut force_keyframe = true;
    let mut pending_monitor_switch = None::<u32>;
    let mut pending_encoding_settings = None::<EncodingSettingsPayload>;
    let mut frame_seq = 0u32;

    let mut stats_interval = Instant::now();
    let mut frames_encoded: u64 = 0;
    let mut total_encode_time_us: u64 = 0;

    log::info!(
        "{} 客户端独立服务启动: monitor {}, {}x{} @{}fps, codec {}",
        transport_name,
        current_monitor_index,
        capturer.width(),
        capturer.height(),
        encoding_settings.fps,
        encoding_settings.codec
    );

    if let Err(e) = send_encoding_settings_state(&runtime, &mut io, encoding_settings) {
        log::warn!("发送初始编码设置失败: {}", e);
        return Ok(());
    }

    loop {
        match drain_control_messages(
            &runtime,
            &mut io,
            &mut force_keyframe,
            &mut pending_monitor_switch,
            &mut pending_encoding_settings,
            input_injector.as_ref(),
            active_monitor,
            transport_name,
        )? {
            ClientConnectionState::Alive => {}
            ClientConnectionState::Closed => {
                log::info!("{} 客户端已断开", transport_name);
                return Ok(());
            }
        }

        if let Some(new_index) = pending_monitor_switch.take() {
            if switch_monitor(
                new_index,
                &mut current_monitor_index,
                &mut capturer,
                &mut encoder,
                encoding_settings,
            )? {
                force_keyframe = true;
                active_monitor = resolve_active_monitor(
                    monitors.as_ref(),
                    current_monitor_index,
                    capturer.width(),
                    capturer.height(),
                );
            }
        }

        if let Some(payload) = pending_encoding_settings.take() {
            if apply_encoding_settings(
                payload,
                &mut encoding_settings,
                &mut encoder,
                capturer.width(),
                capturer.height(),
            ) {
                frame_interval = frame_interval_for_fps(encoding_settings.fps);
                capture_timeout_ms = capture_timeout_ms_for_fps(encoding_settings.fps);
                force_keyframe = true;
            }

            if send_encoding_settings_state(&runtime, &mut io, encoding_settings).is_err() {
                log::info!("{} 客户端已断开", transport_name);
                return Ok(());
            }
        }

        let frame_start = Instant::now();

        let requesting_kf = std::mem::take(&mut force_keyframe);
        if requesting_kf {
            log::info!("客户端请求关键帧");
        }

        let frame_ready = capturer
            .capture_frame(capture_timeout_ms)
            .map_err(|e| e.to_string())?;

        if !frame_ready {
            pace_frame(frame_start, frame_interval);
            continue;
        }

        let nv12_data = capturer.read_nv12().map_err(|e| e.to_string())?;

        let encoded_frames = encoder
            .encode(&nv12_data, requesting_kf)
            .map_err(|e| e.to_string())?;

        for ef in encoded_frames {
            let packet = build_video_packet(&ef.data, frame_seq, ef.pts as u32, ef.is_keyframe);
            frame_seq = frame_seq.wrapping_add(1);

            if send_binary_packet(&runtime, &mut io, packet).is_err() {
                log::info!("{} 客户端已断开", transport_name);
                return Ok(());
            }

            frames_encoded += 1;
            total_encode_time_us += ef.encode_time_us;
        }

        if stats_interval.elapsed() >= Duration::from_secs(5) {
            let elapsed_secs = stats_interval.elapsed().as_secs_f64();
            let avg_encode_ms = if frames_encoded > 0 {
                (total_encode_time_us as f64 / frames_encoded as f64) / 1000.0
            } else {
                0.0
            };
            let encoded_fps = if elapsed_secs > 0.0 {
                frames_encoded as f64 / elapsed_secs
            } else {
                0.0
            };
            log::info!(
                "{} 客户端统计: 已编码 {} 帧, 实际编码帧率: {:.1}fps, 平均编码耗时: {:.2}ms",
                transport_name,
                frames_encoded,
                encoded_fps,
                avg_encode_ms,
            );
            stats_interval = Instant::now();
            frames_encoded = 0;
            total_encode_time_us = 0;
        }

        pace_frame(frame_start, frame_interval);
    }
}

fn send_monitor_list<T: TransportIo>(
    runtime: &tokio::runtime::Handle,
    io: &mut T,
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

    if let Err(e) = send_binary_packet(runtime, io, packet) {
        log::warn!("发送初始显示器列表失败: {}", e);
        return Err(e);
    }
    Ok(())
}

fn send_encoding_settings_state<T: TransportIo>(
    runtime: &tokio::runtime::Handle,
    io: &mut T,
    settings: EncodingSettings,
) -> Result<(), String> {
    let payload = EncodingSettingsStatePayload {
        fps: settings.fps,
        bitrate: settings.bitrate as u32,
        keyframe_interval: settings.keyframe_interval_secs,
        codec: settings.codec.as_client_name(),
    };

    let payload_bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let header = FrameHeader {
        frame_type: FrameType::EncodingSettings,
        flags: FrameFlags::empty(),
        sequence: 0,
        pts: 0,
        payload_len: payload_bytes.len() as u32,
    };

    let mut packet = Vec::with_capacity(FrameHeader::SIZE + payload_bytes.len());
    packet.extend_from_slice(&header.to_bytes());
    packet.extend_from_slice(&payload_bytes);

    send_binary_packet(runtime, io, packet)
}

fn send_binary_packet<T: TransportIo>(
    runtime: &tokio::runtime::Handle,
    io: &mut T,
    packet: Vec<u8>,
) -> Result<(), String> {
    io.send_packet(runtime, packet)
}

fn drain_control_messages<T: TransportIo>(
    runtime: &tokio::runtime::Handle,
    io: &mut T,
    force_keyframe: &mut bool,
    pending_monitor_switch: &mut Option<u32>,
    pending_encoding_settings: &mut Option<EncodingSettingsPayload>,
    input_injector: Option<&InputInjector>,
    active_monitor: ActiveMonitor,
    transport_name: &'static str,
) -> Result<ClientConnectionState, String> {
    loop {
        let maybe_data = match io.recv_packet(runtime, CONTROL_POLL_TIMEOUT) {
            Ok(data) => data,
            Err(e) => {
                log::debug!("{} 接收客户端消息失败: {}", transport_name, e);
                return Ok(ClientConnectionState::Closed);
            }
        };

        let Some(data) = maybe_data else {
            return Ok(ClientConnectionState::Alive);
        };

        handle_binary_control_message(
            &data,
            force_keyframe,
            pending_monitor_switch,
            pending_encoding_settings,
            input_injector,
            active_monitor,
        );
    }
}

fn handle_binary_control_message(
    data: &[u8],
    force_keyframe: &mut bool,
    pending_monitor_switch: &mut Option<u32>,
    pending_encoding_settings: &mut Option<EncodingSettingsPayload>,
    input_injector: Option<&InputInjector>,
    active_monitor: ActiveMonitor,
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
            if let Some(index) = parse_monitor_index(data, header.payload_len) {
                *pending_monitor_switch = Some(index);
            }
        }
        FrameType::EncodingSettings => {
            if let Some(payload) =
                parse_json_payload::<EncodingSettingsPayload>(data, header.payload_len)
            {
                *pending_encoding_settings = Some(payload);
            }
        }
        FrameType::MouseInput => {
            if let (Some(injector), Some(mouse_input)) = (
                input_injector,
                parse_json_payload::<MouseInputPayload>(data, header.payload_len),
            ) {
                if let Err(e) = apply_mouse_input(injector, active_monitor, mouse_input) {
                    log::debug!("处理鼠标输入失败: {}", e);
                }
            }
        }
        FrameType::KeyboardInput => {
            if let (Some(injector), Some(keyboard_input)) = (
                input_injector,
                parse_json_payload::<KeyboardInputPayload>(data, header.payload_len),
            ) {
                if let Err(e) = injector.keyboard_key(
                    keyboard_input.key_code,
                    keyboard_input.code.as_deref(),
                    keyboard_input.down,
                ) {
                    log::debug!("处理键盘输入失败: {}", e);
                }
            }
        }
        _ => {}
    }
}

fn apply_mouse_input(
    injector: &InputInjector,
    active_monitor: ActiveMonitor,
    mouse_input: MouseInputPayload,
) -> Result<(), String> {
    match mouse_input {
        MouseInputPayload::Move { x, y } => injector.move_mouse(active_monitor, x, y),
        MouseInputPayload::Button { x, y, button, down } => {
            injector.mouse_button(active_monitor, x, y, button, down)
        }
        MouseInputPayload::Wheel {
            x,
            y,
            delta_x,
            delta_y,
        } => injector.mouse_wheel(active_monitor, x, y, delta_x, delta_y),
    }
}

fn parse_monitor_index(data: &[u8], payload_len: u32) -> Option<u32> {
    parse_json_payload::<MonitorSelectPayload>(data, payload_len).map(|v| v.index)
}

fn apply_encoding_settings(
    payload: EncodingSettingsPayload,
    encoding_settings: &mut EncodingSettings,
    encoder: &mut AmfEncoder,
    width: u32,
    height: u32,
) -> bool {
    let next_codec = match payload.codec.as_deref() {
        Some(raw_codec) => match VideoCodec::from_client_name(raw_codec) {
            Some(codec) => codec,
            None => {
                log::warn!("忽略未知编码格式: {}", raw_codec);
                encoding_settings.codec
            }
        },
        None => encoding_settings.codec,
    };

    let next_settings = EncodingSettings {
        codec: next_codec,
        fps: payload.fps.clamp(MIN_TARGET_FPS, MAX_TARGET_FPS),
        bitrate: (payload.bitrate as usize).clamp(MIN_TARGET_BITRATE, MAX_TARGET_BITRATE),
        keyframe_interval_secs: payload
            .keyframe_interval
            .clamp(MIN_KEYFRAME_INTERVAL_SECS, MAX_KEYFRAME_INTERVAL_SECS),
    };

    if next_settings.codec == encoding_settings.codec
        && next_settings.fps == encoding_settings.fps
        && next_settings.bitrate == encoding_settings.bitrate
        && next_settings.keyframe_interval_secs == encoding_settings.keyframe_interval_secs
    {
        return false;
    }

    match AmfEncoder::new(&encoder_config(width, height, next_settings)) {
        Ok(new_encoder) => {
            *encoder = new_encoder;
            *encoding_settings = next_settings;
            log::info!(
                "编码设置已更新: {}, {}fps, {}Mbps, 关键帧间隔 {}s",
                next_settings.codec,
                next_settings.fps,
                next_settings.bitrate / 1_000_000,
                next_settings.keyframe_interval_secs
            );
            true
        }
        Err(e) => {
            log::warn!("更新编码设置失败: {}", e);
            false
        }
    }
}

fn parse_json_payload<T: DeserializeOwned>(data: &[u8], payload_len: u32) -> Option<T> {
    let start = FrameHeader::SIZE;
    let end = start.checked_add(payload_len as usize)?;
    if end > data.len() {
        return None;
    }

    serde_json::from_slice(&data[start..end]).ok()
}

fn switch_monitor(
    new_index: u32,
    current_monitor_index: &mut u32,
    capturer: &mut DdaCapture,
    encoder: &mut AmfEncoder,
    encoding_settings: EncodingSettings,
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

    let new_encoder = match AmfEncoder::new(&encoder_config(
        new_capturer.width(),
        new_capturer.height(),
        encoding_settings,
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

fn resolve_active_monitor(
    monitors: &[MonitorInfo],
    monitor_index: u32,
    fallback_width: u32,
    fallback_height: u32,
) -> ActiveMonitor {
    monitors
        .iter()
        .find(|m| m.index == monitor_index)
        .map(ActiveMonitor::from_info)
        .unwrap_or(ActiveMonitor {
            left: 0,
            top: 0,
            width: fallback_width,
            height: fallback_height,
        })
}

fn encoder_config(width: u32, height: u32, settings: EncodingSettings) -> EncoderConfig {
    EncoderConfig {
        codec: settings.codec,
        width,
        height,
        fps: settings.fps,
        bitrate: settings.bitrate,
        keyframe_interval: settings.keyframe_interval_secs,
    }
}

fn frame_interval_for_fps(fps: u32) -> Duration {
    Duration::from_micros(1_000_000 / fps as u64)
}

fn capture_timeout_ms_for_fps(fps: u32) -> u32 {
    // 向上取整并额外加 1ms，降低周期性超时概率
    (1_000u32 + fps - 1) / fps + 1
}

fn build_video_packet(encoded_data: &[u8], sequence: u32, pts: u32, is_keyframe: bool) -> Vec<u8> {
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

fn pace_frame(frame_start: Instant, frame_interval: Duration) {
    let elapsed = frame_start.elapsed();
    if elapsed < frame_interval {
        let sleep_duration = frame_interval - elapsed;
        if sleep_duration > Duration::from_micros(1500) {
            std::thread::sleep(sleep_duration - Duration::from_micros(1500));
        }
        while frame_start.elapsed() < frame_interval {
            std::hint::spin_loop();
        }
    }
}

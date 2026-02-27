use serde::{Deserialize, Serialize};

/// 帧类型标识
///
/// 协议头结构 (固定 16 字节):
/// ```text
/// ┌──────────┬──────────┬──────────┬──────────┬──────────────────┐
/// │ type (1) │ flags(1) │ seq (4)  │ pts (4)  │ payload_len (4)  │  ← 14 bytes header
/// │          │          │          │          │ + 2 reserved      │  ← 16 bytes total
/// ├──────────┴──────────┴──────────┴──────────┴──────────────────┤
/// │                     payload (variable)                        │
/// └──────────────────────────────────────────────────────────────┘
/// ```
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    /// AV1 视频帧
    VideoFrame = 0x01,
    /// 关键帧请求（客户端 → 服务端）
    KeyframeRequest = 0x02,
    /// 统计信息（双向）
    Stats = 0x03,
    /// 心跳包
    Ping = 0x10,
    Pong = 0x11,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct FrameFlags: u8 {
        /// 是否为关键帧
        const KEYFRAME = 0b0000_0001;
        /// 是否为帧的最后一个分片
        const END_OF_FRAME = 0b0000_0010;
    }
}

/// 传输协议头
#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub flags: FrameFlags,
    pub sequence: u32,
    pub pts: u32,
    pub payload_len: u32,
}

impl FrameHeader {
    pub const SIZE: usize = 16;

    /// 序列化为字节
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0] = self.frame_type as u8;
        buf[1] = self.flags.bits();
        buf[2..6].copy_from_slice(&self.sequence.to_le_bytes());
        buf[6..10].copy_from_slice(&self.pts.to_le_bytes());
        buf[10..14].copy_from_slice(&self.payload_len.to_le_bytes());
        // buf[14..16] reserved
        buf
    }

    /// 从字节反序列化
    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Option<Self> {
        let frame_type = match buf[0] {
            0x01 => FrameType::VideoFrame,
            0x02 => FrameType::KeyframeRequest,
            0x03 => FrameType::Stats,
            0x10 => FrameType::Ping,
            0x11 => FrameType::Pong,
            _ => return None,
        };

        Some(Self {
            frame_type,
            flags: FrameFlags::from_bits_truncate(buf[1]),
            sequence: u32::from_le_bytes(buf[2..6].try_into().ok()?),
            pts: u32::from_le_bytes(buf[6..10].try_into().ok()?),
            payload_len: u32::from_le_bytes(buf[10..14].try_into().ok()?),
        })
    }
}

/// 统计信息
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    /// 编码耗时 (微秒)
    pub encode_time_us: u64,
    /// 捕获到发送的延迟 (微秒)
    pub capture_to_send_us: u64,
    /// 当前帧序号
    pub frame_seq: u32,
    /// 服务端时间戳 (微秒, epoch)
    pub server_timestamp_us: u64,
}

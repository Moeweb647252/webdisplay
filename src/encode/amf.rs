use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling;
use ffmpeg_next::{Dictionary, Rational};
use std::fmt;
use std::time::Instant;

/// AMF 视频编码格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Av1,
    H264,
    H265,
}

impl VideoCodec {
    pub fn from_client_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "av1" => Some(Self::Av1),
            "h264" | "avc" => Some(Self::H264),
            "h265" | "hevc" => Some(Self::H265),
            _ => None,
        }
    }

    pub fn as_client_name(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::H264 => "h264",
            Self::H265 => "h265",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Av1 => "AV1",
            Self::H264 => "H.264",
            Self::H265 => "H.265/HEVC",
        }
    }

    fn ffmpeg_encoder_name(self) -> &'static str {
        match self {
            Self::Av1 => "av1_amf",
            Self::H264 => "h264_amf",
            Self::H265 => "hevc_amf",
        }
    }
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// AMF 硬件编码器
pub struct AmfEncoder {
    encoder: ffmpeg::codec::encoder::Video,
    scaler: Option<scaling::Context>,
    frame_index: i64,
    width: u32,
    height: u32,
    src_frame: ffmpeg::frame::Video,
    nv12_frame: ffmpeg::frame::Video,
}

/// 编码后的帧数据
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts: i64,
    pub is_keyframe: bool,
    pub encode_time_us: u64,
}

/// 编码器配置
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 目标码率 (bps)
    pub bitrate: usize,
    /// 关键帧间隔（秒）
    pub keyframe_interval: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 10_000_000,
            keyframe_interval: 2,
        }
    }
}

impl AmfEncoder {
    /// 创建 AMF 编码器
    pub fn new(config: &EncoderConfig) -> Result<Self, Box<dyn std::error::Error>> {
        ffmpeg::init()?;

        let encoder_name = config.codec.ffmpeg_encoder_name();
        let codec = ffmpeg::codec::encoder::find_by_name(encoder_name).ok_or_else(|| {
            format!(
                "找不到 {} 编码器，请确保 FFmpeg 包含 AMF 支持",
                encoder_name
            )
        })?;

        let encoder_ctx = codec::context::Context::new_with_codec(codec);
        let mut video = encoder_ctx.encoder().video()?;

        video.set_width(config.width);
        video.set_height(config.height);
        video.set_format(Pixel::NV12);
        video.set_time_base(Rational::new(1, config.fps as i32));
        video.set_frame_rate(Some(Rational::new(config.fps as i32, 1)));
        video.set_bit_rate(config.bitrate);
        video.set_max_bit_rate(config.bitrate);
        video.set_gop(config.fps * config.keyframe_interval);
        video.set_max_b_frames(0);

        let mut opts = Dictionary::new();
        opts.set("quality", "speed");
        opts.set("rc", "vbr_latency");
        opts.set("frame_skipping", "false");
        opts.set("preanalysis", "false");
        opts.set("preencode", "false");
        opts.set("filler_data", "false");
        opts.set("log_to_dbg", "false");

        match config.codec {
            VideoCodec::Av1 => {
                // AMF AV1 仅支持 lowlatency/transcoding usage
                opts.set("usage", "lowlatency");
                opts.set("header_insertion_mode", "gop");
            }
            VideoCodec::H264 => {
                opts.set("usage", "ultralowlatency");
                opts.set("vbaq", "false");
                opts.set("bf", "0");
            }
            VideoCodec::H265 => {
                opts.set("usage", "ultralowlatency");
                opts.set("vbaq", "false");
                opts.set("header_insertion_mode", "gop");
            }
        }

        let encoder = video.open_with(opts)?;

        let scaler = scaling::Context::get(
            Pixel::BGRA,
            config.width,
            config.height,
            Pixel::NV12,
            config.width,
            config.height,
            scaling::Flags::FAST_BILINEAR,
        )?;

        log::info!(
            "{} AMF 编码器初始化: {}x{} @{}fps, 码率: {} Mbps",
            config.codec,
            config.width,
            config.height,
            config.fps,
            config.bitrate / 1_000_000
        );

        let src_frame = ffmpeg::frame::Video::new(Pixel::BGRA, config.width, config.height);
        let nv12_frame = ffmpeg::frame::Video::new(Pixel::NV12, config.width, config.height);

        Ok(Self {
            encoder,
            scaler: Some(scaler),
            frame_index: 0,
            width: config.width,
            height: config.height,
            src_frame,
            nv12_frame,
        })
    }

    /// 编码一帧 BGRA 数据
    pub fn encode(
        &mut self,
        bgra_data: &[u8],
        stride: u32,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, Box<dyn std::error::Error>> {
        let encode_start = Instant::now();

        let dst_linesize = self.src_frame.stride(0);
        let row_bytes = (self.width * 4) as usize;
        let dst_data = self.src_frame.data_mut(0);

        for y in 0..self.height as usize {
            let src_offset = y * stride as usize;
            let dst_offset = y * dst_linesize;
            dst_data[dst_offset..dst_offset + row_bytes]
                .copy_from_slice(&bgra_data[src_offset..src_offset + row_bytes]);
        }

        if let Some(ref mut scaler) = self.scaler {
            scaler.run(&self.src_frame, &mut self.nv12_frame)?;
        }

        self.nv12_frame.set_pts(Some(self.frame_index));
        if force_keyframe {
            self.nv12_frame.set_kind(ffmpeg::picture::Type::I);
        } else {
            self.nv12_frame.set_kind(ffmpeg::picture::Type::None);
        }
        self.frame_index += 1;

        self.encoder.send_frame(&self.nv12_frame)?;

        let mut encoded_frames = Vec::new();
        let mut packet = ffmpeg::Packet::empty();

        while self.encoder.receive_packet(&mut packet).is_ok() {
            let encode_duration = encode_start.elapsed();
            encoded_frames.push(EncodedFrame {
                data: packet.data().unwrap_or(&[]).to_vec(),
                pts: packet.pts().unwrap_or(0),
                is_keyframe: packet.is_key(),
                encode_time_us: encode_duration.as_micros() as u64,
            });
        }

        Ok(encoded_frames)
    }

    /// 刷新编码器（流结束时调用）
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<Vec<EncodedFrame>, Box<dyn std::error::Error>> {
        self.encoder.send_eof()?;

        let mut encoded_frames = Vec::new();
        let mut packet = ffmpeg::Packet::empty();

        while self.encoder.receive_packet(&mut packet).is_ok() {
            encoded_frames.push(EncodedFrame {
                data: packet.data().unwrap_or(&[]).to_vec(),
                pts: packet.pts().unwrap_or(0),
                is_keyframe: packet.is_key(),
                encode_time_us: 0,
            });
        }

        Ok(encoded_frames)
    }
}

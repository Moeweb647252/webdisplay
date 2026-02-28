use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::software::scaling;
use ffmpeg_next::{Dictionary, Rational};
use std::time::Instant;

/// AV1 AMF 硬件编码器
///
/// 关键编码参数存在码率-质量权衡：更低失真通常需要更高码率。
///
/// 在超低延迟场景下，我们牺牲部分压缩效率以换取编码速度：
/// preset_speed 越偏向 speed，单帧编码耗时通常越低。
pub struct Av1AmfEncoder {
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
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 目标码率 (bps)
    /// 推荐: 1080p@60fps → 8-15 Mbps
    /// 计算公式: B = W * H * fps * bpp
    /// 其中 bpp 约为 0.04 到 0.1 (bits per pixel)
    pub bitrate: usize,
    /// 关键帧间隔（秒）
    /// 超低延迟建议: 1-2 秒
    pub keyframe_interval: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 10_000_000, // 10 Mbps
            keyframe_interval: 2,
        }
    }
}

impl Av1AmfEncoder {
    /// 创建 AV1 AMF 编码器
    ///
    /// 关键低延迟配置：
    /// 1. `usage=ultralowlatency` — AMF 超低延迟预设
    /// 2. `quality=speed` — 最大化编码速度
    /// 3. `latency=lowest_latency` + `async_depth=1` — 限制编码队列深度
    /// 4. `rc=cbr` + `skip_frame=0` — 避免码控主动跳帧
    /// 5. `header_insertion_mode=gop` — 每个 GOP 头插入，支持随时加入
    /// 6. 禁用 B 帧 — B 帧会引入额外重排序延迟，约为 N_B / fps
    pub fn new(config: &EncoderConfig) -> Result<Self, Box<dyn std::error::Error>> {
        ffmpeg::init()?;

        // 查找 AV1 AMF 编码器
        let codec = ffmpeg::codec::encoder::find_by_name("av1_amf")
            .ok_or("找不到 av1_amf 编码器，请确保 FFmpeg 编译时包含 AMF 支持")?;

        let encoder_ctx = codec::context::Context::new_with_codec(codec);
        let mut video = encoder_ctx.encoder().video()?;

        // 基本参数
        video.set_width(config.width);
        video.set_height(config.height);
        video.set_format(Pixel::NV12); // AMF 首选格式
        video.set_time_base(Rational::new(1, config.fps as i32));
        video.set_frame_rate(Some(Rational::new(config.fps as i32, 1)));
        video.set_bit_rate(config.bitrate);
        video.set_max_bit_rate(config.bitrate);
        video.set_gop(config.fps * config.keyframe_interval);
        video.set_max_b_frames(0);

        // AMF 专用低延迟参数
        let mut opts = Dictionary::new();
        opts.set("usage", "ultralowlatency"); // 超低延迟模式
        opts.set("quality", "speed"); // 速度优先
        opts.set("latency", "lowest_latency");
        opts.set("rc", "vbr_latency"); // 码率控制模式，适合低延迟
        opts.set("async_depth", "1");
        opts.set("skip_frame", "0");
        opts.set("preanalysis", "0");
        opts.set("preencode", "0");
        opts.set("bf", "0");
        opts.set("header_insertion_mode", "gop"); // GOP 级别头插入
        opts.set("log_to_dbg", "0");

        let encoder = video.open_with(opts)?;

        // 创建颜色空间转换器 (BGRA → NV12)
        let scaler = scaling::Context::get(
            Pixel::BGRA,
            config.width,
            config.height,
            Pixel::NV12,
            config.width,
            config.height,
            scaling::Flags::FAST_BILINEAR, // 最快的缩放算法
        )?;

        log::info!(
            "AV1 AMF 编码器初始化: {}x{} @{}fps, 码率: {} Mbps",
            config.width,
            config.height,
            config.fps,
            config.bitrate / 1_000_000
        );

        // 预分配帧缓存以供重用
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
    ///
    /// 流程: BGRA 原始帧 → NV12 转换 → AV1 AMF 硬件编码
    ///
    /// 编码延迟由以下因素决定：
    /// T_encode = T_color_convert + T_hw_encode + T_readback
    ///
    /// 其中 T_color_convert 约 0.5ms（SWS 快速双线性），
    /// T_hw_encode 约 3ms（AMF 超低延迟），
    /// T_readback 约 0.5ms
    pub fn encode(
        &mut self,
        bgra_data: &[u8],
        stride: u32,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, Box<dyn std::error::Error>> {
        let encode_start = Instant::now();

        // 将 BGRA 数据填充到预分配的 src_frame 中
        let dst_linesize = self.src_frame.stride(0);
        let row_bytes = (self.width * 4) as usize;
        let dst_data = self.src_frame.data_mut(0);

        for y in 0..self.height as usize {
            let src_offset = y * stride as usize;
            let dst_offset = y * dst_linesize;
            dst_data[dst_offset..dst_offset + row_bytes]
                .copy_from_slice(&bgra_data[src_offset..src_offset + row_bytes]);
        }

        // BGRA → NV12 颜色空间转换，重用 nv12_frame
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

        // 送入编码器
        self.encoder.send_frame(&self.nv12_frame)?;

        // 收集编码输出
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

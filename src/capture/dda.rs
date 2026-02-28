use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// DDA 捕获器 —— 通过 DXGI Output Duplication 实现零拷贝屏幕捕获
pub struct DdaCapture {
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    staging_texture: ID3D11Texture2D,
    cursor_shape: Option<CursorShape>,
    cursor_visible: bool,
    cursor_pos: POINT,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// 捕获到的帧数据
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub stride: u32,
}

/// 缓存的光标形状数据
struct CursorShape {
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    buffer: Vec<u8>,
}

impl DdaCapture {
    /// 枚举所有连接的显示器
    pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, Box<dyn std::error::Error>> {
        let mut monitors = Vec::new();
        unsafe {
            // 创建一个用于枚举的临时设备
            let mut device = None;
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

            if D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .is_err()
            {
                return Ok(monitors);
            }

            if let Some(device) = device {
                let dxgi_device: IDXGIDevice = device.cast()?;
                let adapter = dxgi_device.GetAdapter()?;

                let mut index = 0;
                while let Ok(output) = adapter.EnumOutputs(index) {
                    if let Ok(desc) = output.GetDesc() {
                        let width =
                            (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                        let height =
                            (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                        let primary =
                            desc.DesktopCoordinates.left == 0 && desc.DesktopCoordinates.top == 0;

                        // Parse utf-16 device name
                        let name_len = desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.DeviceName.len());
                        let name = String::from_utf16_lossy(&desc.DeviceName[..name_len]);

                        monitors.push(MonitorInfo {
                            index,
                            name,
                            width,
                            height,
                            primary,
                        });
                    }
                    index += 1;
                }
            }
        }
        Ok(monitors)
    }

    /// 初始化 DDA 捕获
    ///
    /// 延迟关键设计：
    /// - 使用 D3D11_CREATE_DEVICE_VIDEO_SUPPORT 标志以获得硬件加速支持
    /// - Staging 纹理预分配，避免运行时内存分配
    pub fn new(monitor_index: u32) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            // 创建 D3D11 设备
            let mut device = None;
            let mut context = None;
            let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.unwrap();
            let context = context.unwrap();

            // 获取 DXGI 适配器和输出
            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let output: IDXGIOutput = adapter.EnumOutputs(monitor_index)?;
            let output1: IDXGIOutput1 = output.cast()?;

            // 获取输出描述以得到分辨率
            let desc = output.GetDesc()?;
            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

            // 创建 Desktop Duplication
            let duplication = output1.DuplicateOutput(&device)?;

            // 预分配 staging 纹理（CPU 可读）
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };

            let mut staging_texture = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging_texture))?;
            let staging_texture = staging_texture.unwrap();

            log::info!("DDA 捕获器初始化完成: {}x{}, 格式: BGRA8", width, height);

            Ok(Self {
                context,
                duplication,
                width,
                height,
                staging_texture,
                cursor_shape: None,
                cursor_visible: false,
                cursor_pos: POINT::default(),
            })
        }
    }

    /// 捕获一帧
    ///
    /// 超时时间设为 0ms 实现非阻塞轮询，
    /// 或设为较短时间如 16ms（一帧时间）以降低 CPU 占用。
    ///
    /// 帧获取时间复杂度: $O(1)$ GPU 拷贝 + $O(W \times H)$ CPU 映射
    pub fn capture_frame(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Option<CapturedFrame>, Box<dyn std::error::Error>> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            // 尝试获取下一帧
            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code().0 as u32 == 0x887A0027 => {
                    // DXGI_ERROR_WAIT_TIMEOUT
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }

            // 更新光标形状（仅当形状发生变化时）
            if frame_info.PointerShapeBufferSize > 0 {
                let buf_size = frame_info.PointerShapeBufferSize;
                let mut shape_buffer = vec![0u8; buf_size as usize];
                let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                let mut size_needed = 0u32;
                if self
                    .duplication
                    .GetFramePointerShape(
                        buf_size,
                        shape_buffer.as_mut_ptr() as *mut _,
                        &mut size_needed,
                        &mut shape_info,
                    )
                    .is_ok()
                {
                    self.cursor_shape = Some(CursorShape {
                        info: shape_info,
                        buffer: shape_buffer,
                    });
                }
            }

            // 更新光标位置（仅当鼠标状态发生变化时）
            if frame_info.LastMouseUpdateTime != 0 {
                self.cursor_visible = frame_info.PointerPosition.Visible.as_bool();
                self.cursor_pos = frame_info.PointerPosition.Position;
            }

            let resource = resource.unwrap();
            let texture: ID3D11Texture2D = resource.cast()?;

            // GPU → Staging 拷贝（GPU 内部操作，非常快）
            self.context.CopyResource(&self.staging_texture, &texture);

            // 映射 staging 纹理到 CPU 内存
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(
                &self.staging_texture,
                0,
                D3D11_MAP_READ,
                0,
                Some(&mut mapped),
            )?;

            // 拷贝像素数据
            let stride = mapped.RowPitch;
            let data_size = (stride * self.height) as usize;
            let src_slice = std::slice::from_raw_parts(mapped.pData as *const u8, data_size);
            let mut data = src_slice.to_vec();

            self.context.Unmap(&self.staging_texture, 0);
            self.duplication.ReleaseFrame()?;

            // 将光标叠加混合到帧数据上
            if self.cursor_visible {
                if let Some(ref shape) = self.cursor_shape {
                    draw_cursor_on_frame(
                        &mut data,
                        stride,
                        self.width,
                        self.height,
                        self.cursor_pos,
                        &shape.info,
                        &shape.buffer,
                    );
                }
            }

            Ok(Some(CapturedFrame { data, stride }))
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// 将光标叠加到帧缓冲（BGRA8 格式）
///
/// 支持 DXGI 三种光标类型：
/// - **MONOCHROME**：1-bpp AND/XOR 双掩码，高度为实际高度的两倍
/// - **COLOR**：32-bit BGRA，标准预乘 Alpha 混合
/// - **MASKED_COLOR**：32-bit BGRA，`alpha=0x00` 透明，`alpha=0xFF` 与桌面 XOR，其余 Alpha 混合
fn draw_cursor_on_frame(
    frame: &mut [u8],
    frame_stride: u32,
    frame_width: u32,
    frame_height: u32,
    cursor_pos: POINT,
    shape_info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    shape_buf: &[u8],
) {
    // 光标图像左上角 = 热点屏幕坐标 − 热点偏移
    let left = cursor_pos.x - shape_info.HotSpot.x;
    let top = cursor_pos.y - shape_info.HotSpot.y;

    if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        // 上半部分：AND 掩码；下半部分：XOR 掩码；均为 1-bpp，行宽 Pitch 字节
        let cursor_h = (shape_info.Height / 2) as i32;
        let cursor_w = shape_info.Width as i32;
        let pitch = shape_info.Pitch as usize;

        for cy in 0..cursor_h {
            let fy = top + cy;
            if fy < 0 || fy >= frame_height as i32 {
                continue;
            }
            for cx in 0..cursor_w {
                let fx = left + cx;
                if fx < 0 || fx >= frame_width as i32 {
                    continue;
                }

                let byte_off = cy as usize * pitch + cx as usize / 8;
                let bit_mask = 0x80u8 >> (cx as u32 % 8);
                let and_bit = shape_buf.get(byte_off).map_or(1u8, |b| u8::from(b & bit_mask != 0));
                let xor_bit = shape_buf
                    .get(byte_off + cursor_h as usize * pitch)
                    .map_or(0u8, |b| u8::from(b & bit_mask != 0));

                let fp = fy as usize * frame_stride as usize + fx as usize * 4;
                if fp + 2 >= frame.len() {
                    continue;
                }

                // screen = (screen AND and) XOR xor  (每通道)
                // and=0,xor=0 → 黑; and=1,xor=0 → 透明; and=0,xor=1 → 白; and=1,xor=1 → 反色
                if and_bit == 0 {
                    let val = xor_bit * 255;
                    frame[fp] = val;
                    frame[fp + 1] = val;
                    frame[fp + 2] = val;
                } else if xor_bit == 1 {
                    frame[fp] = !frame[fp];
                    frame[fp + 1] = !frame[fp + 1];
                    frame[fp + 2] = !frame[fp + 2];
                }
            }
        }
    } else if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        // 32-bit BGRA，标准 Alpha 混合
        let cursor_h = shape_info.Height as i32;
        let cursor_w = shape_info.Width as i32;
        let pitch = shape_info.Pitch as usize;

        for cy in 0..cursor_h {
            let fy = top + cy;
            if fy < 0 || fy >= frame_height as i32 {
                continue;
            }
            for cx in 0..cursor_w {
                let fx = left + cx;
                if fx < 0 || fx >= frame_width as i32 {
                    continue;
                }

                let cp = cy as usize * pitch + cx as usize * 4;
                let Some(&[cb, cg, cr, ca]) = shape_buf.get(cp..cp + 4)
                    .and_then(|s| s.try_into().ok())
                    .map(|a: &[u8; 4]| a)
                else {
                    continue;
                };
                if ca == 0 {
                    continue;
                }

                let fp = fy as usize * frame_stride as usize + fx as usize * 4;
                if fp + 2 >= frame.len() {
                    continue;
                }

                if ca == 255 {
                    frame[fp] = cb;
                    frame[fp + 1] = cg;
                    frame[fp + 2] = cr;
                } else {
                    let a = ca as u32;
                    let ia = 255 - a;
                    frame[fp] = ((cb as u32 * a + frame[fp] as u32 * ia) / 255) as u8;
                    frame[fp + 1] = ((cg as u32 * a + frame[fp + 1] as u32 * ia) / 255) as u8;
                    frame[fp + 2] = ((cr as u32 * a + frame[fp + 2] as u32 * ia) / 255) as u8;
                }
            }
        }
    } else if shape_info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        // alpha=0xFF → XOR 桌面像素；alpha=0x00 → 透明；其余 → Alpha 混合
        let cursor_h = shape_info.Height as i32;
        let cursor_w = shape_info.Width as i32;
        let pitch = shape_info.Pitch as usize;

        for cy in 0..cursor_h {
            let fy = top + cy;
            if fy < 0 || fy >= frame_height as i32 {
                continue;
            }
            for cx in 0..cursor_w {
                let fx = left + cx;
                if fx < 0 || fx >= frame_width as i32 {
                    continue;
                }

                let cp = cy as usize * pitch + cx as usize * 4;
                let Some(&[cb, cg, cr, ca]) = shape_buf.get(cp..cp + 4)
                    .and_then(|s| s.try_into().ok())
                    .map(|a: &[u8; 4]| a)
                else {
                    continue;
                };

                let fp = fy as usize * frame_stride as usize + fx as usize * 4;
                if fp + 2 >= frame.len() {
                    continue;
                }

                match ca {
                    0x00 => { /* 透明，不修改 */ }
                    0xFF => {
                        frame[fp] ^= cb;
                        frame[fp + 1] ^= cg;
                        frame[fp + 2] ^= cr;
                    }
                    a => {
                        let a = a as u32;
                        let ia = 255 - a;
                        frame[fp] = ((cb as u32 * a + frame[fp] as u32 * ia) / 255) as u8;
                        frame[fp + 1] = ((cg as u32 * a + frame[fp + 1] as u32 * ia) / 255) as u8;
                        frame[fp + 2] = ((cr as u32 * a + frame[fp + 2] as u32 * ia) / 255) as u8;
                    }
                }
            }
        }
    }
}

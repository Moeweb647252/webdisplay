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
}

/// 捕获到的帧数据
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub stride: u32,
}

impl DdaCapture {
    /// 初始化 DDA 捕获
    ///
    /// 延迟关键设计：
    /// - 使用 D3D11_CREATE_DEVICE_VIDEO_SUPPORT 标志以获得硬件加速支持
    /// - Staging 纹理预分配，避免运行时内存分配
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
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
            let output: IDXGIOutput = adapter.EnumOutputs(0)?;
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
            let data = src_slice.to_vec();

            self.context.Unmap(&self.staging_texture, 0);
            self.duplication.ReleaseFrame()?;

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

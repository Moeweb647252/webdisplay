use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{BOOL, Interface, PCSTR};

const ROTATION_IDENTITY: u32 = 0;
const ROTATION_90: u32 = 1;
const ROTATION_180: u32 = 2;
const ROTATION_270: u32 = 3;

const CURSOR_TYPE_NONE: u32 = 0;
const CURSOR_TYPE_COLOR: u32 = 1;
const CURSOR_TYPE_MASKED_COLOR: u32 = 2;
const CURSOR_TYPE_MONOCHROME: u32 = 3;

const COMPOSITE_SHADER: &str = r#"
cbuffer CompositeCB : register(b0) {
    uint4 src_info;
    int4 cursor_rect;
    uint4 cursor_info;
};

Texture2D<float4> frame_tex : register(t0);
Texture2D<float4> cursor_color_tex : register(t1);
Texture2D<uint> cursor_mono_tex : register(t2);

struct VsOut {
    float4 position : SV_POSITION;
};

VsOut vs_main(uint vertex_id : SV_VertexID) {
    float2 pos = float2(-1.0, -1.0);
    if (vertex_id == 1) {
        pos = float2(-1.0, 3.0);
    } else if (vertex_id == 2) {
        pos = float2(3.0, -1.0);
    }

    VsOut outv;
    outv.position = float4(pos, 0.0, 1.0);
    return outv;
}

float4 ps_main(VsOut input) : SV_TARGET {
    int2 dst = int2(input.position.xy);
    int src_w = (int)src_info.x;
    int src_h = (int)src_info.y;
    int rotation = (int)src_info.z;

    int2 src = dst;
    if (rotation == 1) {
        src = int2(dst.y, src_h - 1 - dst.x);
    } else if (rotation == 2) {
        src = int2(src_w - 1 - dst.x, src_h - 1 - dst.y);
    } else if (rotation == 3) {
        src = int2(src_w - 1 - dst.y, dst.x);
    }

    float4 base = frame_tex.Load(int3(src, 0));
    base.a = 1.0;

    if (src_info.w != 0u) {
        int cx = dst.x - cursor_rect.x;
        int cy = dst.y - cursor_rect.y;
        if (cx >= 0 && cy >= 0 && cx < cursor_rect.z && cy < cursor_rect.w) {
            uint cursor_type = cursor_info.x;
            if (cursor_type == 1u) {
                float4 c = cursor_color_tex.Load(int3(cx, cy, 0));
                float a = saturate(c.a);
                base.rgb = c.rgb * a + base.rgb * (1.0 - a);
            } else if (cursor_type == 2u) {
                float4 c = cursor_color_tex.Load(int3(cx, cy, 0));
                uint ca = (uint)round(saturate(c.a) * 255.0);
                if (ca == 255u) {
                    uint3 src_u8 = uint3(round(saturate(base.rgb) * 255.0));
                    uint3 mask_u8 = uint3(round(saturate(c.rgb) * 255.0));
                    src_u8 = src_u8 ^ mask_u8;
                    base.rgb = float3(src_u8) / 255.0;
                } else if (ca != 0u) {
                    float a = (float)ca / 255.0;
                    base.rgb = c.rgb * a + base.rgb * (1.0 - a);
                }
            } else if (cursor_type == 3u) {
                uint op = cursor_mono_tex.Load(int3(cx, cy, 0));
                if (op == 0u) {
                    base.rgb = float3(0.0, 0.0, 0.0);
                } else if (op == 2u) {
                    base.rgb = float3(1.0, 1.0, 1.0);
                } else if (op == 3u) {
                    base.rgb = 1.0 - base.rgb;
                }
            }
        }
    }

    return base;
}
"#;

/// DDA 捕获器 —— DXGI Output Duplication + D3D11 Video Processor BGRA→NV12 全 GPU 管线
pub struct DdaCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    phys_width: u32,
    phys_height: u32,
    shader_rotation: u32,
    frame_texture: ID3D11Texture2D,
    frame_srv: ID3D11ShaderResourceView,
    composed_texture: ID3D11Texture2D,
    composed_rtv: ID3D11RenderTargetView,
    /// Video Processor 输出的 NV12 纹理（GPU 色彩转换结果）
    nv12_texture: ID3D11Texture2D,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    constant_buffer: ID3D11Buffer,
    viewport: D3D11_VIEWPORT,
    cursor_shape: Option<CursorShape>,
    cursor_visible: bool,
    cursor_pos: POINT,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    video_processor_enum: ID3D11VideoProcessorEnumerator,
    video_processor: ID3D11VideoProcessor,
    vp_output_view: ID3D11VideoProcessorOutputView,
    vp_input_view: ID3D11VideoProcessorInputView,
    /// 预分配的 NV12 staging 纹理（避免每帧重新创建）
    staging_texture: ID3D11Texture2D,
    /// 预分配的 NV12 读取缓冲区（避免每帧重新分配 Vec）
    nv12_read_buf: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

struct CursorShape {
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    width: u32,
    height: u32,
    texture: CursorTexture,
}

enum CursorTexture {
    Color(ID3D11ShaderResourceView),
    MaskedColor(ID3D11ShaderResourceView),
    Monochrome(ID3D11ShaderResourceView),
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompositeConstants {
    src_info: [u32; 4],
    cursor_rect: [i32; 4],
    cursor_info: [u32; 4],
}

impl DdaCapture {
    /// 枚举所有连接的显示器
    pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, Box<dyn std::error::Error>> {
        let mut monitors = Vec::new();
        unsafe {
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

                        let name_len = desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.DeviceName.len());
                        let name = String::from_utf16_lossy(&desc.DeviceName[..name_len]);

                        monitors.push(MonitorInfo {
                            index,
                            name,
                            left: desc.DesktopCoordinates.left,
                            top: desc.DesktopCoordinates.top,
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
    pub fn new(monitor_index: u32) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
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

            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let output: IDXGIOutput = adapter.EnumOutputs(monitor_index)?;
            let output1: IDXGIOutput1 = output.cast()?;

            let desc = output.GetDesc()?;
            let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
            let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

            let duplication = output1.DuplicateOutput(&device)?;
            let dupl_desc = duplication.GetDesc();
            let rotation = dupl_desc.Rotation;

            let (phys_width, phys_height) =
                if dupl_desc.ModeDesc.Width > 0 && dupl_desc.ModeDesc.Height > 0 {
                    (dupl_desc.ModeDesc.Width, dupl_desc.ModeDesc.Height)
                } else if rotation == DXGI_MODE_ROTATION_ROTATE90
                    || rotation == DXGI_MODE_ROTATION_ROTATE270
                {
                    (height, width)
                } else {
                    (width, height)
                };
            let shader_rotation = to_shader_rotation(rotation);

            let frame_desc = D3D11_TEXTURE2D_DESC {
                Width: phys_width,
                Height: phys_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut frame_texture = None;
            device.CreateTexture2D(&frame_desc, None, Some(&mut frame_texture))?;
            let frame_texture = frame_texture.unwrap();

            let mut frame_srv = None;
            device.CreateShaderResourceView(&frame_texture, None, Some(&mut frame_srv))?;
            let frame_srv = frame_srv.unwrap();

            let composed_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut composed_texture = None;
            device.CreateTexture2D(&composed_desc, None, Some(&mut composed_texture))?;
            let composed_texture = composed_texture.unwrap();

            let mut composed_rtv = None;
            device.CreateRenderTargetView(&composed_texture, None, Some(&mut composed_rtv))?;
            let composed_rtv = composed_rtv.unwrap();

            // ── NV12 输出纹理（Video Processor 写入，编码器读取）──
            let nv12_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut nv12_texture = None;
            device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12_texture))?;
            let nv12_texture = nv12_texture.unwrap();

            // ── D3D11 Video Processor 初始化 ──
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;

            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let video_processor_enum =
                video_device.CreateVideoProcessorEnumerator(&content_desc as *const _)?;

            let video_processor = video_device.CreateVideoProcessor(&video_processor_enum, 0)?;

            let in_color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (0 & 1)
                    | ((0 & 1) << 1)
                    | ((1 & 1) << 2)
                    | ((0 & 1) << 3)
                    | ((2 & 3) << 4), // Nominal Range: 2 (0-255)
            };
            let out_color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (0 & 1)
                    | ((0 & 1) << 1)
                    | ((1 & 1) << 2)
                    | ((0 & 1) << 3)
                    | ((1 & 3) << 4), // Nominal Range: 1 (16-235)
            };
            video_context.VideoProcessorSetStreamColorSpace(&video_processor, 0, &in_color_space);
            video_context.VideoProcessorSetOutputColorSpace(&video_processor, &out_color_space);

            // 输出视图（NV12 纹理）
            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view = None;
            video_device.CreateVideoProcessorOutputView(
                &nv12_texture,
                &video_processor_enum,
                &output_view_desc as *const _,
                Some(&mut out_view),
            )?;
            let vp_output_view = out_view.unwrap();

            // 输入视图（BGRA composed_texture）
            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut in_view = None;
            video_device.CreateVideoProcessorInputView(
                &composed_texture,
                &video_processor_enum,
                &input_view_desc as *const _,
                Some(&mut in_view),
            )?;
            let vp_input_view = in_view.unwrap();

            let vs_blob = compile_shader_blob(COMPOSITE_SHADER, b"vs_main\0", b"vs_5_0\0")?;
            let ps_blob = compile_shader_blob(COMPOSITE_SHADER, b"ps_main\0", b"ps_5_0\0")?;

            let mut vertex_shader = None;
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vertex_shader))?;
            let vertex_shader = vertex_shader.unwrap();

            let mut pixel_shader = None;
            device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut pixel_shader))?;
            let pixel_shader = pixel_shader.unwrap();

            let constant_desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<CompositeConstants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut constant_buffer = None;
            device.CreateBuffer(&constant_desc, None, Some(&mut constant_buffer))?;
            let constant_buffer = constant_buffer.unwrap();

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };

            // ── 预分配 Staging 纹理（避免每帧 CreateTexture2D 开销）──
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
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

            // 预分配 NV12 读取缓冲区: Y (w*h) + UV (w*h/2)
            let nv12_buf_size = (width as usize) * (height as usize) * 3 / 2;
            let nv12_read_buf = vec![0u8; nv12_buf_size];

            log::info!(
                "DDA 捕获器初始化完成: 逻辑 {}x{}, 物理 {}x{}, 旋转 {}, GPU 合成启用",
                width,
                height,
                phys_width,
                phys_height,
                rotation.0
            );

            Ok(Self {
                device,
                context,
                duplication,
                width,
                height,
                phys_width,
                phys_height,
                shader_rotation,
                frame_texture,
                frame_srv,
                composed_texture,
                composed_rtv,
                nv12_texture,
                vertex_shader,
                pixel_shader,
                constant_buffer,
                viewport,
                cursor_shape: None,
                cursor_visible: false,
                cursor_pos: POINT::default(),
                video_device,
                video_context,
                video_processor_enum,
                video_processor,
                vp_output_view,
                vp_input_view,
                staging_texture,
                nv12_read_buf,
            })
        }
    }

    /// 捕获一帧并通过 GPU Video Processor 转换为 NV12，写入 self.nv12_texture
    /// 返回 true 表示新帧已就绪，false 表示超时无新帧
    pub fn capture_frame(&mut self, timeout_ms: u32) -> Result<bool, Box<dyn std::error::Error>> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code().0 as u32 == 0x887A0027 => {
                    return Ok(false); // 超时，无新帧
                }
                Err(e) => return Err(e.into()),
            }

            if frame_info.PointerShapeBufferSize > 0 {
                self.update_cursor_shape(frame_info.PointerShapeBufferSize)?;
            }

            if frame_info.LastMouseUpdateTime != 0 {
                self.cursor_visible = frame_info.PointerPosition.Visible.as_bool();
                self.cursor_pos = frame_info.PointerPosition.Position;
            }

            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let resource = resource.ok_or("AcquireNextFrame 未返回帧资源")?;
                let texture: ID3D11Texture2D = resource.cast()?;

                // GPU 合成（旋转校正 + 光标叠加）→ composed_texture (BGRA)
                self.context.CopyResource(&self.frame_texture, &texture);
                self.render_composite()?;

                // GPU Video Processor: BGRA → NV12（写入 nv12_texture）
                let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: BOOL(1),
                    OutputIndex: 0,
                    InputFrameOrField: 0,
                    PastFrames: 0,
                    FutureFrames: 0,
                    ppPastSurfaces: std::ptr::null_mut(),
                    pInputSurface: ManuallyDrop::new(Some(self.vp_input_view.clone())),
                    ppFutureSurfaces: std::ptr::null_mut(),
                    ppPastSurfacesRight: std::ptr::null_mut(),
                    pInputSurfaceRight: ManuallyDrop::new(None),
                    ppFutureSurfacesRight: std::ptr::null_mut(),
                };
                self.video_context.VideoProcessorBlt(
                    &self.video_processor,
                    &self.vp_output_view,
                    0,
                    &[stream],
                )?;

                Ok(())
            })();

            self.duplication.ReleaseFrame()?;

            match result {
                Ok(()) => Ok(true),
                Err(e) => Err(e),
            }
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// 返回最新捕获已转换的 NV12 纹理（供编码器直接使用）
    #[allow(dead_code)]
    pub fn nv12_texture(&self) -> &ID3D11Texture2D {
        &self.nv12_texture
    }

    /// 返回 D3D11 device（供编码器共享，建立 hw_frames_ctx）
    #[allow(dead_code)]
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// 将 nv12_texture 经由预分配的 Staging 纹理回读到 CPU，返回完整 NV12 字节流
    /// 布局：Y 面 (width×height) 字节 + UV 面 (width×height/2) 字节（交错）
    pub fn read_nv12(&mut self) -> Result<&[u8], Box<dyn std::error::Error>> {
        unsafe {
            self.context
                .CopyResource(&self.staging_texture, &self.nv12_texture);

            // NV12 staging textures: map subresource 0 only.
            // Both planes are accessible from the single mapped pointer:
            //   Y  rows: pData + row * RowPitch
            //   UV rows: pData + RowPitch*Height + row * RowPitch
            // Mapping subresource 1 separately returns E_INVALIDARG on most drivers.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(
                &self.staging_texture,
                0,
                D3D11_MAP_READ,
                0,
                Some(&mut mapped),
            )?;

            let w = self.width as usize;
            let h = self.height as usize;
            let row_pitch = mapped.RowPitch as usize;
            let base_ptr = mapped.pData as *const u8;

            if row_pitch == w {
                // Fast path: stride == width, 直接连续拷贝整个 Y+UV 面
                let total = w * h + w * (h / 2);
                let src = std::slice::from_raw_parts(base_ptr, total);
                self.nv12_read_buf[..total].copy_from_slice(src);
            } else {
                // Slow path: stride != width, 逐行拷贝
                // Y plane — rows 0..h
                for row in 0..h {
                    let src = std::slice::from_raw_parts(base_ptr.add(row * row_pitch), w);
                    self.nv12_read_buf[row * w..(row + 1) * w].copy_from_slice(src);
                }

                // UV plane — interleaved, rows 0..h/2, starts at RowPitch * Height
                let uv_base = base_ptr.add(row_pitch * h);
                let uv_start = w * h;
                for row in 0..(h / 2) {
                    let src = std::slice::from_raw_parts(uv_base.add(row * row_pitch), w);
                    self.nv12_read_buf[uv_start + row * w..uv_start + (row + 1) * w]
                        .copy_from_slice(src);
                }
            }

            self.context.Unmap(&self.staging_texture, 0);
            Ok(&self.nv12_read_buf)
        }
    }

    fn update_cursor_shape(
        &mut self,
        shape_buffer_size: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        unsafe {
            let mut shape_buffer = vec![0u8; shape_buffer_size as usize];
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut size_needed = 0u32;

            self.duplication.GetFramePointerShape(
                shape_buffer_size,
                shape_buffer.as_mut_ptr() as *mut _,
                &mut size_needed,
                &mut shape_info,
            )?;

            match create_cursor_shape(&self.device, shape_info, &shape_buffer) {
                Ok(shape) => {
                    self.cursor_shape = Some(shape);
                }
                Err(e) => {
                    self.cursor_shape = None;
                    log::warn!("创建 GPU 光标纹理失败: {}", e);
                }
            }

            Ok(())
        }
    }

    fn render_composite(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        unsafe {
            let mut constants = CompositeConstants {
                src_info: [
                    self.phys_width,
                    self.phys_height,
                    self.shader_rotation,
                    CURSOR_TYPE_NONE,
                ],
                ..Default::default()
            };

            let mut color_srv = None;
            let mut mono_srv = None;

            if self.cursor_visible {
                if let Some(shape) = &self.cursor_shape {
                    constants.src_info[3] = 1;
                    constants.cursor_rect = [
                        self.cursor_pos.x - shape.info.HotSpot.x,
                        self.cursor_pos.y - shape.info.HotSpot.y,
                        shape.width as i32,
                        shape.height as i32,
                    ];

                    match &shape.texture {
                        CursorTexture::Color(srv) => {
                            constants.cursor_info[0] = CURSOR_TYPE_COLOR;
                            color_srv = Some(srv.clone());
                        }
                        CursorTexture::MaskedColor(srv) => {
                            constants.cursor_info[0] = CURSOR_TYPE_MASKED_COLOR;
                            color_srv = Some(srv.clone());
                        }
                        CursorTexture::Monochrome(srv) => {
                            constants.cursor_info[0] = CURSOR_TYPE_MONOCHROME;
                            mono_srv = Some(srv.clone());
                        }
                    }
                }
            }

            self.context.UpdateSubresource(
                &self.constant_buffer,
                0,
                None,
                &constants as *const _ as *const c_void,
                0,
                0,
            );

            self.context.IASetInputLayout(None::<&ID3D11InputLayout>);
            self.context
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.RSSetViewports(Some(&[self.viewport]));
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.pixel_shader, None);

            let constant_buffers = [Some(self.constant_buffer.clone())];
            self.context
                .PSSetConstantBuffers(0, Some(&constant_buffers));

            let render_targets = [Some(self.composed_rtv.clone())];
            self.context
                .OMSetRenderTargets(Some(&render_targets), None::<&ID3D11DepthStencilView>);

            let shader_resources = [Some(self.frame_srv.clone()), color_srv, mono_srv];
            self.context
                .PSSetShaderResources(0, Some(&shader_resources));

            self.context.Draw(3, 0);

            let empty_srvs: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];
            self.context.PSSetShaderResources(0, Some(&empty_srvs));

            let empty_rtvs: [Option<ID3D11RenderTargetView>; 1] = [None];
            self.context
                .OMSetRenderTargets(Some(&empty_rtvs), None::<&ID3D11DepthStencilView>);

            Ok(())
        }
    }
}

fn to_shader_rotation(rotation: DXGI_MODE_ROTATION) -> u32 {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 {
        ROTATION_90
    } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
        ROTATION_180
    } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
        ROTATION_270
    } else {
        ROTATION_IDENTITY
    }
}

fn create_cursor_shape(
    device: &ID3D11Device,
    shape_info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    shape_buffer: &[u8],
) -> Result<CursorShape, Box<dyn std::error::Error>> {
    let shape_type = shape_info.Type;

    if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        let height = shape_info.Height;
        let required = shape_info.Pitch as usize * height as usize;
        if shape_buffer.len() < required {
            return Err("COLOR 光标数据长度不足".into());
        }

        let srv = create_cursor_srv(
            device,
            shape_info.Width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            &shape_buffer[..required],
            shape_info.Pitch,
        )?;

        Ok(CursorShape {
            info: shape_info,
            width: shape_info.Width,
            height,
            texture: CursorTexture::Color(srv),
        })
    } else if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        let height = shape_info.Height;
        let required = shape_info.Pitch as usize * height as usize;
        if shape_buffer.len() < required {
            return Err("MASKED_COLOR 光标数据长度不足".into());
        }

        let srv = create_cursor_srv(
            device,
            shape_info.Width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            &shape_buffer[..required],
            shape_info.Pitch,
        )?;

        Ok(CursorShape {
            info: shape_info,
            width: shape_info.Width,
            height,
            texture: CursorTexture::MaskedColor(srv),
        })
    } else if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        let width = shape_info.Width;
        let height = shape_info.Height / 2;
        if width == 0 || height == 0 {
            return Err("MONOCHROME 光标尺寸无效".into());
        }

        let pitch = shape_info.Pitch as usize;
        let required = pitch * shape_info.Height as usize;
        if shape_buffer.len() < required {
            return Err("MONOCHROME 光标数据长度不足".into());
        }

        let mut ops = vec![0u8; (width * height) as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let byte_off = y * pitch + x / 8;
                let bit_mask = 0x80u8 >> (x as u32 % 8);

                let and_bit = u8::from(shape_buffer[byte_off] & bit_mask != 0);
                let xor_bit =
                    u8::from(shape_buffer[byte_off + height as usize * pitch] & bit_mask != 0);

                ops[y * width as usize + x] = and_bit + (xor_bit << 1);
            }
        }

        let srv = create_cursor_srv(device, width, height, DXGI_FORMAT_R8_UINT, &ops, width)?;

        Ok(CursorShape {
            info: shape_info,
            width,
            height,
            texture: CursorTexture::Monochrome(srv),
        })
    } else {
        Err(format!("不支持的光标类型: {}", shape_type).into())
    }
}

fn create_cursor_srv(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    data: &[u8],
    pitch: u32,
) -> Result<ID3D11ShaderResourceView, Box<dyn std::error::Error>> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let subresource = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const c_void,
            SysMemPitch: pitch,
            SysMemSlicePitch: pitch * height,
        };

        let mut texture = None;
        device.CreateTexture2D(&desc, Some(&subresource), Some(&mut texture))?;
        let texture = texture.unwrap();

        let mut srv = None;
        device.CreateShaderResourceView(&texture, None, Some(&mut srv))?;
        srv.ok_or("创建光标 SRV 失败".into())
    }
}

fn compile_shader_blob(
    source: &str,
    entry: &'static [u8],
    target: &'static [u8],
) -> Result<ID3DBlob, Box<dyn std::error::Error>> {
    unsafe {
        let mut shader_blob = None;
        let mut error_blob = None;

        let compile_result = D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut shader_blob,
            Some(&mut error_blob),
        );

        if let Err(err) = compile_result {
            let compiler_msg = error_blob
                .as_ref()
                .map(blob_to_string)
                .unwrap_or_else(|| err.to_string());
            let stage =
                std::str::from_utf8(&entry[..entry.len().saturating_sub(1)]).unwrap_or("shader");
            return Err(format!("编译 {} 失败: {}", stage, compiler_msg).into());
        }

        shader_blob.ok_or("着色器编译结果为空".into())
    }
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn blob_to_string(blob: &ID3DBlob) -> String {
    unsafe {
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .to_string()
    }
}

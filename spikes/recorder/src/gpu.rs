use std::mem::{ManuallyDrop, size_of};
use std::time::Duration;

use igorio_core::geom::Rect;
use windows::Win32::Graphics::{
    Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Fxc::D3DCompile, ID3DBlob,
        ID3DInclude,
    },
    Direct3D11::*,
    Dxgi::{Common::*, IDXGIAdapter},
};
use windows::core::{Interface, PCSTR, s};

use crate::{FPS, Result, require};

pub type Size = (u32, u32);

pub struct CursorLayer {
    pub rect: [f32; 4],
    pub views: [Option<ID3D11ShaderResourceView>; 2],
}

pub struct Gpu {
    pub device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    input_size: Size,
    output_size: Size,
    desktop: ID3D11Texture2D,
    desktop_view: ID3D11ShaderResourceView,
    target: ID3D11RenderTargetView,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    video: Video,
}

impl Gpu {
    pub fn new(adapter: &IDXGIAdapter, input_size: Size, output_size: Size) -> Result<Self> {
        let (mut device, mut ctx) = (None, None);
        // SAFETY: the selected adapter is live and both output slots are writable.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
        }
        let device = require(device, "D3D device")?;
        let ctx = require(ctx, "D3D immediate context")?;
        let multithread: ID3D11Multithread = ctx.cast()?;
        // SAFETY: Media Foundation may share this device. Protect immediate-context access.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        let desktop = texture(
            &device,
            input_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE,
            None,
        )?;
        let desktop_view = view(&device, &desktop)?;
        let target_tex = texture(
            &device,
            output_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET,
            None,
        )?;
        let mut target = None;
        // SAFETY: the BGRA texture has render-target bind flags and the default view is valid.
        unsafe { device.CreateRenderTargetView(&target_tex, None, Some(&mut target))? };
        let video = Video::new(&device, &ctx, &target_tex, output_size)?;

        let mut constants = None;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<[f32; 8]>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: the 32-byte constant buffer descriptor and output slot are local.
        unsafe { device.CreateBuffer(&desc, None, Some(&mut constants))? };

        let mut sampler = None;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxAnisotropy: 1,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        // SAFETY: local sampler descriptor and output slot stay alive through the call.
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler))? };

        let vs_bytecode = compile(s!("vs"), s!("vs_5_0"))?;
        let ps_bytecode = compile(s!("ps"), s!("ps_5_0"))?;
        let (mut vs, mut ps) = (None, None);
        // SAFETY: the compiled bytecode is alive and matches each shader stage.
        unsafe {
            device.CreateVertexShader(bytes(&vs_bytecode), None, Some(&mut vs))?;
            device.CreatePixelShader(bytes(&ps_bytecode), None, Some(&mut ps))?;
        }
        Ok(Self {
            device,
            ctx,
            input_size,
            output_size,
            desktop,
            desktop_view,
            video,
            target: require(target, "BGRA render-target view")?,
            constants: require(constants, "constant buffer")?,
            sampler: require(sampler, "bilinear sampler")?,
            vs: require(vs, "vertex shader")?,
            ps: require(ps, "pixel shader")?,
        })
    }

    pub fn update_desktop(&self, source: &ID3D11Texture2D) {
        // SAFETY: the capture surface and retained texture have the same size, device, and format.
        unsafe { self.ctx.CopyResource(&self.desktop, source) };
    }

    pub fn normalize(&self, area: Rect) -> [f32; 4] {
        let w = f64::from(self.input_size.0);
        let h = f64::from(self.input_size.1);
        [
            (area.x / w) as f32,
            (area.y / h) as f32,
            (area.width / w) as f32,
            (area.height / h) as f32,
        ]
    }

    pub fn render(&self, area: Rect, cursor: Option<CursorLayer>) -> Result<ID3D11Texture2D> {
        let mut params = [0.0; 8];
        params[..4].copy_from_slice(&self.normalize(area));
        let mut views = [Some(self.desktop_view.clone()), None, None];
        if let Some(cursor) = cursor {
            params[4..].copy_from_slice(&cursor.rect);
            let [black, white] = cursor.views;
            views[1] = black;
            views[2] = white;
        }
        let viewport = D3D11_VIEWPORT {
            Width: self.output_size.0 as f32,
            Height: self.output_size.1 as f32,
            MaxDepth: 1.0,
            ..Default::default()
        };
        // SAFETY: all bound objects belong to this device and the constants fill the buffer.
        unsafe {
            self.ctx
                .UpdateSubresource(&self.constants, 0, None, params.as_ptr().cast(), 0, 0);
            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.ctx.RSSetViewports(Some(&[viewport]));
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx.PSSetShader(&self.ps, None);
            self.ctx
                .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.ctx
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.PSSetShaderResources(0, Some(&views));
            self.ctx
                .OMSetRenderTargets(Some(&[Some(self.target.clone())]), None);
            self.ctx.Draw(3, 0);
            self.ctx.OMSetRenderTargets(None, None);
            self.ctx.PSSetShaderResources(0, Some(&[None, None, None]));
        }
        self.video.convert(&self.device, self.output_size)
    }

    pub fn upload_cursor(&self, size: Size, bgra: &[u8]) -> Result<ID3D11ShaderResourceView> {
        if bgra.len() != size.0 as usize * size.1 as usize * 4 {
            return Err("invalid cursor bitmap size".into());
        }
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr().cast(),
            SysMemPitch: size.0 * 4,
            SysMemSlicePitch: 0,
        };
        let image = texture(
            &self.device,
            size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE,
            Some(&data),
        )?;
        view(&self.device, &image)
    }
}

struct Video {
    device: ID3D11VideoDevice,
    ctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
}

impl Video {
    fn new(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        bgra: &ID3D11Texture2D,
        size: Size,
    ) -> Result<Self> {
        let device: ID3D11VideoDevice = device.cast()?;
        let ctx: ID3D11VideoContext = ctx.cast()?;
        let colors: ID3D11VideoContext1 = ctx.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: FPS as u32,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            OutputFrameRate: rate,
            InputWidth: size.0,
            InputHeight: size.1,
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: descriptor remains live for the call. This is a progressive 1:1 conversion.
        let enumerator = unsafe { device.CreateVideoProcessorEnumerator(&desc)? };
        // SAFETY: rate conversion index zero is the baseline mode of this enumerator.
        let processor = unsafe { device.CreateVideoProcessor(&enumerator, 0)? };
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut input = None;
        // SAFETY: bgra is the same device's single-slice render target.
        unsafe {
            device.CreateVideoProcessorInputView(bgra, &enumerator, &desc, Some(&mut input))?
        };
        // SAFETY: configure only stream zero, with matching SDR RGB and YUV color spaces.
        unsafe {
            ctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            colors.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            colors.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
        }
        Ok(Self {
            device,
            ctx,
            enumerator,
            processor,
            input: require(input, "video processor input view")?,
        })
    }

    fn convert(&self, device: &ID3D11Device, size: Size) -> Result<ID3D11Texture2D> {
        // Each sample owns its texture until the writer and encoder release it.
        let nv12 = texture(
            device,
            size,
            DXGI_FORMAT_NV12,
            D3D11_BIND_RENDER_TARGET,
            None,
        )?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut output = None;
        // SAFETY: the single-slice NV12 render target matches the enumerator's dimensions.
        unsafe {
            self.device.CreateVideoProcessorOutputView(
                &nv12,
                &self.enumerator,
                &desc,
                Some(&mut output),
            )?
        };
        let output = require(output, "video processor output view")?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        // SAFETY: both surfaces are live. All optional past/future streams are empty.
        let result = unsafe {
            self.ctx
                .VideoProcessorBlt(&self.processor, &output, 0, std::slice::from_ref(&stream))
        };
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        result?;
        Ok(nv12)
    }
}

fn texture(
    device: &ID3D11Device,
    size: Size,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
    data: Option<&D3D11_SUBRESOURCE_DATA>,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: the descriptors and output slot are live. Optional initial data covers the texture.
    unsafe { device.CreateTexture2D(&desc, data.map(std::ptr::from_ref), Some(&mut texture))? };
    require(texture, "D3D texture")
}

fn view(device: &ID3D11Device, texture: &ID3D11Texture2D) -> Result<ID3D11ShaderResourceView> {
    let mut view = None;
    // SAFETY: this device owns the shader-resource texture and can infer its view format.
    unsafe { device.CreateShaderResourceView(texture, None, Some(&mut view))? };
    require(view, "shader-resource view")
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let source = include_bytes!("composite.hlsl");
    let (mut code, mut error) = (None, None);
    // SAFETY: the source is sized explicitly, entry and target are C strings, and outputs are local.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("composite.hlsl"),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut error),
        )
    };
    if let Err(err) = result {
        return Err(format!(
            "{err}: {}",
            error
                .as_ref()
                .map(|blob| String::from_utf8_lossy(bytes(blob)))
                .unwrap_or_default()
        )
        .into());
    }
    require(code, "compiled shader")
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: D3DCompiler owns the nonempty byte buffer for the lifetime of its blob.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

#[derive(Default)]
pub struct Timing {
    pub count: u64,
    total: Duration,
    worst: Duration,
}

impl Timing {
    pub fn add(&mut self, elapsed: Duration) {
        self.count += 1;
        self.total += elapsed;
        self.worst = self.worst.max(elapsed);
    }

    pub fn print(&self, name: &str) {
        let avg = self.total.as_secs_f64() * 1000.0 / self.count.max(1) as f64;
        println!(
            "{name}: avg {avg:.3} ms, worst {:.3} ms ({})",
            self.worst.as_secs_f64() * 1000.0,
            self.count
        );
    }
}

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::{PCSTR, s};

use crate::{Error, Result};

const HLSL: &str = "
struct VO { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VO vs(uint id : SV_VertexID) {
    VO o;
    float2 uv = float2(id & 1, id >> 1);
    o.pos = float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
    o.uv = uv;
    return o;
}
Texture2D t : register(t0);
float4 ps(VO i) : SV_Target {
    uint w, h;
    t.GetDimensions(w, h);
    return t.Load(int3(min(uint2(i.uv * float2(w, h)), uint2(w - 1, h - 1)), 0));
}
";

const SHAPE_MONOCHROME: u32 = 1;
const SHAPE_MASKED_COLOR: u32 = 4;

struct Layers {
    normal: Option<ID3D11ShaderResourceView>,
    invert: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
}

pub(super) struct Cursor {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend_normal: ID3D11BlendState,
    blend_invert: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    layers: Option<Layers>,
    normal_px: Vec<u8>,
    invert_px: Vec<u8>,
    pub(super) visible: bool,
    pub(super) pos: (i32, i32),
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut err: Option<ID3DBlob> = None;
    // SAFETY: el código fuente vive durante la llamada y los punteros de salida son locales.
    let r = unsafe {
        D3DCompile(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            None,
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut err),
        )
    };
    match (r, code) {
        (Ok(()), Some(c)) => Ok(c),
        _ => {
            let msg = err
                .map(|e| {
                    // SAFETY: el blob de errores contiene texto ASCII de longitud conocida.
                    let b = unsafe {
                        std::slice::from_raw_parts(
                            e.GetBufferPointer().cast::<u8>(),
                            e.GetBufferSize(),
                        )
                    };
                    String::from_utf8_lossy(b).into_owned()
                })
                .unwrap_or_default();
            Err(Error::Media(format!(
                "no se pudo compilar el sombreador del cursor: {msg}"
            )))
        }
    }
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    // SAFETY: el blob es dueño del búfer mientras viva la referencia.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }
}

fn blend_state(
    device: &ID3D11Device,
    src: D3D11_BLEND,
    dst: D3D11_BLEND,
) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: src,
        DestBlend: dst,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ONE,
        DestBlendAlpha: D3D11_BLEND_ZERO,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut state = None;
    // SAFETY: descripción válida y puntero de salida local.
    unsafe { device.CreateBlendState(&desc, Some(&mut state))? };
    state.ok_or_else(|| Error::Media("no se pudo crear el estado de mezcla".into()))
}

impl Cursor {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_blob = compile(s!("vs"), s!("vs_4_0"))?;
        let ps_blob = compile(s!("ps"), s!("ps_4_0"))?;
        let (mut vs, mut ps, mut raster) = (None, None, None);
        let rdesc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        // SAFETY: bytecode recién compilado y punteros de salida locales.
        unsafe {
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))?;
            device.CreateRasterizerState(&rdesc, Some(&mut raster))?;
        }
        let missing = || Error::Media("no se pudo crear el estado gráfico del cursor".into());
        Ok(Self {
            vs: vs.ok_or_else(missing)?,
            ps: ps.ok_or_else(missing)?,
            raster: raster.ok_or_else(missing)?,
            blend_normal: blend_state(device, D3D11_BLEND_ONE, D3D11_BLEND_INV_SRC_ALPHA)?,
            blend_invert: blend_state(
                device,
                D3D11_BLEND_INV_DEST_COLOR,
                D3D11_BLEND_INV_SRC_COLOR,
            )?,
            layers: None,
            normal_px: Vec::new(),
            invert_px: Vec::new(),
            visible: false,
            pos: (0, 0),
        })
    }

    pub(super) fn set_shape(
        &mut self,
        device: &ID3D11Device,
        kind: u32,
        width: u32,
        height: u32,
        pitch: u32,
        data: &[u8],
    ) -> Result<()> {
        let (w, h) = (width as usize, height as usize);
        let pitch = pitch as usize;
        let rows = if kind == SHAPE_MONOCHROME { h / 2 } else { h };
        if w == 0 || rows == 0 {
            self.layers = None;
            return Ok(());
        }
        let needed = match kind {
            SHAPE_MONOCHROME => pitch * rows * 2,
            _ => pitch * rows,
        };
        if data.len() < needed {
            return Err(Error::Media("forma de cursor truncada".into()));
        }
        self.normal_px.clear();
        self.normal_px.resize(w * rows * 4, 0);
        self.invert_px.clear();
        self.invert_px.resize(w * rows * 4, 0);
        let (mut any_normal, mut any_invert) = (false, false);
        for y in 0..rows {
            for x in 0..w {
                let o = (y * w + x) * 4;
                match kind {
                    SHAPE_MONOCHROME => {
                        let bit = 0x80u8 >> (x % 8);
                        let and = data[y * pitch + x / 8] & bit != 0;
                        let xor = data[(y + rows) * pitch + x / 8] & bit != 0;
                        match (and, xor) {
                            (false, false) => {
                                self.normal_px[o + 3] = 255;
                                any_normal = true;
                            }
                            (false, true) => {
                                self.normal_px[o..o + 4].fill(255);
                                any_normal = true;
                            }
                            (true, true) => {
                                self.invert_px[o..o + 3].fill(255);
                                any_invert = true;
                            }
                            (true, false) => {}
                        }
                    }
                    SHAPE_MASKED_COLOR => {
                        let p = &data[y * pitch + x * 4..y * pitch + x * 4 + 4];
                        if p[3] == 0 {
                            self.normal_px[o..o + 3].copy_from_slice(&p[..3]);
                            self.normal_px[o + 3] = 255;
                            any_normal = true;
                        } else {
                            self.invert_px[o..o + 3].copy_from_slice(&p[..3]);
                            any_invert = true;
                        }
                    }
                    _ => {
                        let p = &data[y * pitch + x * 4..y * pitch + x * 4 + 4];
                        let a = p[3] as u32;
                        if a != 0 {
                            for (dst, &src) in self.normal_px[o..o + 3].iter_mut().zip(p) {
                                *dst = (src as u32 * a / 255) as u8;
                            }
                            self.normal_px[o + 3] = p[3];
                            any_normal = true;
                        }
                    }
                }
            }
        }
        let normal = if any_normal {
            Some(make_layer(device, width, rows as u32, &self.normal_px)?)
        } else {
            None
        };
        let invert = if any_invert {
            Some(make_layer(device, width, rows as u32, &self.invert_px)?)
        } else {
            None
        };
        self.layers = Some(Layers {
            normal,
            invert,
            width,
            height: rows as u32,
        });
        Ok(())
    }

    pub(super) fn draw(
        &self,
        ctx: &ID3D11DeviceContext,
        target: &ID3D11RenderTargetView,
        target_w: u32,
        target_h: u32,
    ) {
        let Some(l) = self.layers.as_ref().filter(|_| self.visible) else {
            return;
        };
        let (x, y) = self.pos;
        if x >= target_w as i32
            || y >= target_h as i32
            || x + l.width as i32 <= 0
            || y + l.height as i32 <= 0
        {
            return;
        }
        let vp = D3D11_VIEWPORT {
            TopLeftX: x as f32,
            TopLeftY: y as f32,
            Width: l.width as f32,
            Height: l.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: estado del contexto inmediato con objetos propios; el llamador mantiene
        // el contexto bloqueado (ID3D11Multithread::Enter) durante toda la composición.
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.RSSetViewports(Some(&[vp]));
            ctx.RSSetState(&self.raster);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            for (layer, blend) in [
                (&l.normal, &self.blend_normal),
                (&l.invert, &self.blend_invert),
            ] {
                if let Some(srv) = layer {
                    ctx.OMSetBlendState(blend, None, 0xffff_ffff);
                    ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                    ctx.Draw(4, 0);
                }
            }
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(Some(&[None]), None);
        }
    }
}

fn make_layer(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let (mut tex, mut srv) = (None, None);
    // SAFETY: los píxeles viven durante la llamada y los punteros de salida son locales.
    unsafe {
        device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        let tex =
            tex.ok_or_else(|| Error::Media("no se pudo crear la textura del cursor".into()))?;
        device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
    }
    srv.ok_or_else(|| Error::Media("no se pudo crear la vista del cursor".into()))
}

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_GRADIENT_STOP, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_CAP_STYLE_ROUND, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_ELLIPSE,
    D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_EXTEND_MODE_CLAMP, D2D1_GAMMA_2_2, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_LAYER_OPTIONS1_NONE, D2D1_LAYER_PARAMETERS1, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES1, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Factory, ID2D1PathGeometry, ID2D1RenderTarget, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush,
    ID2D1StrokeStyle,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{Interface, PCWSTR, Result, w};
use windows_numerics::{Matrix3x2, Vector2};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }

    // 0xAARRGGBB, the notation used by the WinUI theme resources.
    pub const fn argb(v: u32) -> Self {
        Self::rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8)
    }

    pub fn alpha(self, a: f32) -> Self {
        Self { a: self.a * a, ..self }
    }

    pub fn mix(self, other: Color, t: f32) -> Self {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Self { r: l(self.r, other.r), g: l(self.g, other.g), b: l(self.b, other.b), a: l(self.a, other.a) }
    }

    fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.r, g: self.g, b: self.b, a: self.a }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Font {
    Caption,
    Body,
    BodyStrong,
    BodyLarge,
    Subtitle,
    Title,
    Display,
    Icon,
    Icon20,
    IconSmall,
    IconLarge,
    IconHuge,
    IconCaption,
    Mono,
    Quote,
}

impl Font {
    const ALL: [Font; 15] = [
        Font::Caption,
        Font::Body,
        Font::BodyStrong,
        Font::BodyLarge,
        Font::Subtitle,
        Font::Title,
        Font::Display,
        Font::Icon,
        Font::Icon20,
        Font::IconSmall,
        Font::IconLarge,
        Font::IconHuge,
        Font::IconCaption,
        Font::Mono,
        Font::Quote,
    ];

    fn spec(self) -> (PCWSTR, f32, DWRITE_FONT_WEIGHT) {
        let text = w!("Segoe UI Variable Text");
        let display = w!("Segoe UI Variable Display");
        let icons = w!("Segoe Fluent Icons");
        match self {
            Font::Caption => (w!("Segoe UI Variable Small"), 12.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Body => (text, 14.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::BodyStrong => (text, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::BodyLarge => (display, 18.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Subtitle => (display, 20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Title => (display, 28.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Display => (display, 40.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Icon => (icons, 16.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Icon20 => (icons, 20.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::IconSmall => (icons, 12.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::IconLarge => (icons, 24.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::IconHuge => (icons, 48.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::IconCaption => (icons, 10.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Quote => (w!("Segoe UI Variable Text"), 15.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Mono => (w!("Cascadia Mono"), 12.0, DWRITE_FONT_WEIGHT_NORMAL),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Start,
    Center,
    End,
}

impl Align {
    fn dwrite(self) -> DWRITE_TEXT_ALIGNMENT {
        match self {
            Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
            Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
        }
    }
}

struct Formats {
    single: HashMap<Font, IDWriteTextFormat>,
    wrapped: HashMap<Font, IDWriteTextFormat>,
}

pub struct Gfx {
    _d3d: ID3D11Device,
    swapchain: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    dwrite: IDWriteFactory,
    wic: IWICImagingFactory,
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle,
    formats: Formats,
    width: u32,
    height: u32,
    dpi: f32,
    wide: RefCell<Vec<u16>>,
}

fn create_device(kind: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            kind,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

impl Gfx {
    pub fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> Result<Self> {
        let d3d = create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        let dxgi: IDXGIDevice = d3d.cast()?;
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain = factory.CreateSwapChainForComposition(&d3d, &desc, None)?;
            let d2d: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&D2D1_FACTORY_OPTIONS::default()))?;
            let dc = d2d.CreateDevice(&dxgi)?.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let brush = dc.CreateSolidColorBrush(&Color::TRANSPARENT.d2d(), None)?;
            let round = d2d.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES1 {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            let formats = create_formats(&dwrite)?;
            let gfx = Self {
                _d3d: d3d,
                swapchain,
                dc,
                _dcomp: dcomp,
                _target: target,
                _visual: visual,
                dwrite,
                wic,
                brush,
                round: round.cast()?,
                formats,
                width: width.max(1),
                height: height.max(1),
                dpi,
                wide: RefCell::new(Vec::with_capacity(256)),
            };
            gfx.bind()?;
            Ok(gfx)
        }
    }

    fn bind(&self) -> Result<()> {
        unsafe {
            let surface: IDXGISurface = self.swapchain.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: self.dpi,
                dpiY: self.dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            let bitmap = self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.dc.SetTarget(&bitmap);
            self.dc.SetDpi(self.dpi, self.dpi);
        }
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32, dpi: f32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.width && height == self.height && dpi == self.dpi {
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.dpi = dpi;
        unsafe {
            self.dc.SetTarget(None);
            self.swapchain.ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        self.bind()
    }

    pub fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    pub fn size_dip(&self) -> (f32, f32) {
        (self.width as f32 / self.scale(), self.height as f32 / self.scale())
    }

    pub fn begin(&self, background: Color) {
        unsafe {
            self.dc.BeginDraw();
            self.dc.SetTransform(&Matrix3x2::identity());
            self.dc.Clear(Some(&background.d2d()));
        }
    }

    pub fn end(&self) -> Result<()> {
        unsafe {
            self.dc.EndDraw(None, None)?;
            self.swapchain.Present(1, DXGI_PRESENT(0)).ok()
        }
    }

    pub fn load_png(&self, bytes: &[u8]) -> Result<ID2D1Bitmap1> {
        unsafe {
            let stream = self.wic.CreateStream()?;
            stream.InitializeFromMemory(bytes)?;
            let decoder = self.wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
            let frame = decoder.GetFrame(0)?;
            let converter = self.wic.CreateFormatConverter()?;
            converter.Initialize(
                &frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;
            self.dc.CreateBitmapFromWicBitmap(&converter, None)
        }
    }

    // Bitmap from premultiplied BGRA pixels (top-down, tightly packed).
    pub fn bitmap_from_bgra(&self, width: u32, height: u32, pixels: &[u8]) -> Result<ID2D1Bitmap1> {
        unsafe {
            let source = self.wic.CreateBitmapFromMemory(width, height, &GUID_WICPixelFormat32bppPBGRA, width * 4, pixels)?;
            self.dc.CreateBitmapFromWicBitmap(&source, None)
        }
    }

    pub fn image(&self, bitmap: &ID2D1Bitmap1, rect: Rect, opacity: f32) {
        unsafe {
            self.dc.DrawBitmap(
                bitmap,
                Some(&rect.d2d()),
                opacity,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
    }

    fn set(&self, color: Color) {
        unsafe { self.brush.SetColor(&color.d2d()) };
    }

    pub fn fill(&self, rect: Rect, radius: f32, color: Color) {
        if color.a <= 0.0 {
            return;
        }
        self.set(color);
        unsafe {
            if radius > 0.0 {
                let rr = D2D1_ROUNDED_RECT { rect: rect.d2d(), radiusX: radius, radiusY: radius };
                self.dc.FillRoundedRectangle(&rr, &self.brush);
            } else {
                self.dc.FillRectangle(&rect.d2d(), &self.brush);
            }
        }
    }

    // Strokes inside `rect` so a 1px border lines up with a fill of the same rect.
    pub fn stroke(&self, rect: Rect, radius: f32, width: f32, color: Color) {
        if color.a <= 0.0 {
            return;
        }
        self.set(color);
        let r = rect.inset(width / 2.0, width / 2.0);
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: (radius - width / 2.0).max(0.0), radiusY: (radius - width / 2.0).max(0.0) };
        unsafe { self.dc.DrawRoundedRectangle(&rr, &self.brush, width, None) };
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, color: Color) {
        self.set(color);
        let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: r, radiusY: r };
        unsafe { self.dc.FillEllipse(&e, &self.brush) };
    }

    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Color) {
        self.set(color);
        unsafe {
            self.dc.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &self.brush, width, &self.round);
        }
    }

    #[allow(clippy::too_many_arguments)]
    // Indeterminate progress ring: an arc of `sweep` radians starting at `start`.
    pub fn arc(&self, cx: f32, cy: f32, r: f32, start: f32, sweep: f32, width: f32, color: Color) {
        self.set(color);
        let steps = 24;
        let mut prev = (cx + r * start.cos(), cy + r * start.sin());
        for i in 1..=steps {
            let a = start + sweep * i as f32 / steps as f32;
            let next = (cx + r * a.cos(), cy + r * a.sin());
            unsafe {
                self.dc.DrawLine(Vector2 { X: prev.0, Y: prev.1 }, Vector2 { X: next.0, Y: next.1 }, &self.brush, width, &self.round);
            }
            prev = next;
        }
    }

    // Soft radial glow fading from `color` at the centre to transparent at `radius`.
    pub fn glow(&self, cx: f32, cy: f32, radius: f32, color: Color) {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: color.d2d() },
            D2D1_GRADIENT_STOP { position: 1.0, color: color.alpha(0.0).d2d() },
        ];
        unsafe {
            let target: &ID2D1RenderTarget = &self.dc;
            let Ok(collection) = target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else {
                return;
            };
            let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                center: Vector2 { X: cx, Y: cy },
                gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                radiusX: radius,
                radiusY: radius,
            };
            let Ok(brush) = self.dc.CreateRadialGradientBrush(&props, None, &collection) else { return };
            let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
            self.dc.FillEllipse(&e, &brush);
        }
    }

    // Elliptical glow, for wide soft light such as the hero background.
    pub fn glow_ellipse(&self, cx: f32, cy: f32, rx: f32, ry: f32, color: Color) {
        if color.a <= 0.0 || rx <= 0.0 || ry <= 0.0 {
            return;
        }
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: color.d2d() },
            D2D1_GRADIENT_STOP { position: 0.55, color: color.alpha(0.45).d2d() },
            D2D1_GRADIENT_STOP { position: 1.0, color: color.alpha(0.0).d2d() },
        ];
        unsafe {
            let target: &ID2D1RenderTarget = &self.dc;
            let Ok(collection) = target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else {
                return;
            };
            let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                center: Vector2 { X: cx, Y: cy },
                gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                radiusX: rx,
                radiusY: ry,
            };
            let Ok(brush) = self.dc.CreateRadialGradientBrush(&props, None, &collection) else { return };
            let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rx, radiusY: ry };
            self.dc.FillEllipse(&e, &brush);
        }
    }

    // Rounded rect filled with a linear gradient from `from` (top-left) to `to` (bottom-right).
    pub fn gradient(&self, rect: Rect, radius: f32, from: Color, to: Color) {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: from.d2d() },
            D2D1_GRADIENT_STOP { position: 1.0, color: to.d2d() },
        ];
        unsafe {
            let target: &ID2D1RenderTarget = &self.dc;
            let Ok(collection) = target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) else {
                return;
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: rect.x, Y: rect.y },
                endPoint: Vector2 { X: rect.right(), Y: rect.bottom() },
            };
            let Ok(brush) = self.dc.CreateLinearGradientBrush(&props, None, &collection) else { return };
            let rr = D2D1_ROUNDED_RECT { rect: rect.d2d(), radiusX: radius, radiusY: radius };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    // Point on the quadratic curve p0 → p2 bent towards p1, at t in 0..1.
    pub fn curve_point(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), t: f32) -> (f32, f32) {
        let u = 1.0 - t;
        (u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0, u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1)
    }

    // Quadratic curve drawn from t0 to t1 (fractions of its length) with round caps.
    #[allow(clippy::too_many_arguments)]
    pub fn curve(&self, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), t0: f32, t1: f32, width: f32, color: Color) {
        if color.a <= 0.0 || t1 <= t0 {
            return;
        }
        self.set(color);
        let steps = (((t1 - t0) * 40.0) as usize).max(2);
        let mut prev = Self::curve_point(p0, p1, p2, t0);
        for i in 1..=steps {
            let next = Self::curve_point(p0, p1, p2, t0 + (t1 - t0) * i as f32 / steps as f32);
            unsafe {
                self.dc.DrawLine(Vector2 { X: prev.0, Y: prev.1 }, Vector2 { X: next.0, Y: next.1 }, &self.brush, width, &self.round);
            }
            prev = next;
        }
    }

    // Starts scaling everything drawn next by `k` around (cx, cy); give the returned matrix to
    // `end_scale` to go back.
    pub fn begin_scale(&self, cx: f32, cy: f32, k: f32) -> Matrix3x2 {
        // SAFETY: lectura y escritura de la transformación del contexto propio.
        unsafe {
            let mut current = Matrix3x2::identity();
            self.dc.GetTransform(&mut current);
            self.dc.SetTransform(&(Matrix3x2::translation(-cx, -cy) * Matrix3x2::scale(k, k) * Matrix3x2::translation(cx, cy) * current));
            current
        }
    }

    pub fn end_scale(&self, saved: Matrix3x2) {
        // SAFETY: restaura la transformación guardada por `begin_scale`.
        unsafe { self.dc.SetTransform(&saved) };
    }

    pub fn factory(&self) -> Option<ID2D1Factory> {
        unsafe { self.dc.GetFactory().ok() }
    }

    // Fills a 24×24 vector icon (see svg.rs) at (x, y) scaled to `size`, keeping any active transform.
    pub fn fill_icon(&self, geometry: &ID2D1PathGeometry, x: f32, y: f32, size: f32, color: Color) {
        self.set(color);
        unsafe {
            let mut current = Matrix3x2::identity();
            self.dc.GetTransform(&mut current);
            let k = size / 24.0;
            self.dc.SetTransform(&(Matrix3x2::scale(k, k) * Matrix3x2::translation(x, y) * current));
            self.dc.FillGeometry(geometry, &self.brush, None);
            self.dc.SetTransform(&current);
        }
    }

    pub fn push_clip(&self, rect: Rect) {
        unsafe { self.dc.PushAxisAlignedClip(&rect.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
    }

    pub fn pop_clip(&self) {
        unsafe { self.dc.PopAxisAlignedClip() };
    }

    // Everything drawn until `pop_layer` is composited with `opacity` and shifted by `dy`.
    pub fn push_layer(&self, opacity: f32, dy: f32) {
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: D2D_RECT_F { left: -1.0e6, top: -1.0e6, right: 1.0e6, bottom: 1.0e6 },
            geometricMask: std::mem::ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity,
            opacityBrush: std::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe {
            self.dc.PushLayer(&params, None);
            self.dc.SetTransform(&Matrix3x2::translation(0.0, dy));
        }
    }

    pub fn pop_layer(&self) {
        unsafe {
            self.dc.SetTransform(&Matrix3x2::identity());
            self.dc.PopLayer();
        }
    }

    fn format(&self, font: Font, wrap: bool) -> &IDWriteTextFormat {
        if wrap { &self.formats.wrapped[&font] } else { &self.formats.single[&font] }
    }

    fn encode(&self, text: &str) -> std::cell::RefMut<'_, Vec<u16>> {
        let mut wide = self.wide.borrow_mut();
        wide.clear();
        wide.extend(text.encode_utf16());
        wide
    }

    pub fn text(&self, text: &str, font: Font, rect: Rect, color: Color, align: Align) {
        self.draw_text(text, font, rect, color, align, false, true);
    }

    pub fn text_top(&self, text: &str, font: Font, rect: Rect, color: Color, align: Align) {
        self.draw_text(text, font, rect, color, align, false, false);
    }

    pub fn paragraph(&self, text: &str, font: Font, rect: Rect, color: Color) {
        self.draw_text(text, font, rect, color, Align::Start, true, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(&self, text: &str, font: Font, rect: Rect, color: Color, align: Align, wrap: bool, center: bool) {
        if text.is_empty() || color.a <= 0.0 {
            return;
        }
        let format = self.format(font, wrap);
        self.set(color);
        let wide = self.encode(text);
        unsafe {
            let _ = format.SetTextAlignment(align.dwrite());
            let _ = format.SetParagraphAlignment(if center {
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER
            } else {
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR
            });
            self.dc.DrawText(
                &wide,
                format,
                &rect.d2d(),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn glyph(&self, glyph: char, font: Font, cx: f32, cy: f32, color: Color) {
        let mut buf = [0u8; 4];
        self.text(glyph.encode_utf8(&mut buf), font, Rect::new(cx - 30.0, cy - 30.0, 60.0, 60.0), color, Align::Center);
    }

    // Returns (width, height) of `text` laid out within `max_width`.
    pub fn measure(&self, text: &str, font: Font, max_width: f32, wrap: bool) -> (f32, f32) {
        let format = self.format(font, wrap).clone();
        let wide = self.encode(text);
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let Ok(layout) = self.dwrite.CreateTextLayout(&wide, &format, max_width, 10_000.0) else {
                return (0.0, 0.0);
            };
            let mut metrics = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut metrics).is_err() {
                return (0.0, 0.0);
            }
            (metrics.widthIncludingTrailingWhitespace, metrics.height)
        }
    }
}

fn create_formats(dwrite: &IDWriteFactory) -> Result<Formats> {
    let mut single = HashMap::new();
    let mut wrapped = HashMap::new();
    for font in Font::ALL {
        let (family, size, weight) = font.spec();
        unsafe {
            let make = || {
                dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    if font == Font::Quote { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL },
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("es-es"),
                )
            };
            let one = make()?;
            one.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            let sign = dwrite.CreateEllipsisTrimmingSign(&one)?;
            one.SetTrimming(&trimming, &sign)?;
            let many = make()?;
            many.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
            single.insert(font, one);
            wrapped.insert(font, many);
        }
    }
    Ok(Formats { single, wrapped })
}

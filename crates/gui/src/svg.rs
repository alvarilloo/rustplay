// Turns SVG path data ("M12 .297c-6.63 0-12 …") into a Direct2D geometry, so vector logos stay
// sharp at any size and DPI.
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_BEZIER_SEGMENT, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_FILL_MODE_WINDING,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_QUADRATIC_BEZIER_SEGMENT,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE, ID2D1Factory, ID2D1GeometrySink,
    ID2D1PathGeometry,
};
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::core::Result;
use windows_numerics::Vector2;

struct Tokens<'a> {
    s: &'a [u8],
    i: usize,
}

impl Tokens<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b',' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn command(&mut self) -> Option<u8> {
        self.skip();
        let c = *self.s.get(self.i)?;
        if c.is_ascii_alphabetic() {
            self.i += 1;
            Some(c)
        } else {
            None
        }
    }

    fn has_number(&mut self) -> bool {
        self.skip();
        self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
    }

    fn number(&mut self) -> Option<f32> {
        self.skip();
        let start = self.i;
        let mut seen_dot = false;
        let mut seen_exp = false;
        if matches!(self.s.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'0'..=b'9' => self.i += 1,
                b'.' if !seen_dot && !seen_exp => {
                    seen_dot = true;
                    self.i += 1;
                }
                b'e' | b'E' if !seen_exp => {
                    seen_exp = true;
                    self.i += 1;
                    if matches!(self.s.get(self.i), Some(b'-' | b'+')) {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }

    // Arc flags are a single 0/1 and may be written without separators ("a5 5 0 00-.6-.2").
    fn flag(&mut self) -> Option<bool> {
        self.skip();
        let c = *self.s.get(self.i)?;
        self.i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    }
}

fn point(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

pub fn geometry(factory: &ID2D1Factory, data: &str) -> Result<ID2D1PathGeometry> {
    unsafe {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.SetFillMode(D2D1_FILL_MODE_WINDING);
        build(&sink, data);
        sink.Close()?;
        Ok(path)
    }
}

fn build(sink: &ID2D1GeometrySink, data: &str) {
    let mut t = Tokens { s: data.as_bytes(), i: 0 };
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    let mut last_ctrl: Option<(f32, f32, u8)> = None;
    let mut open = false;
    let mut command = b'M';
    unsafe {
        loop {
            if let Some(c) = t.command() {
                command = c;
            } else if !t.has_number() {
                break;
            }
            let relative = command.is_ascii_lowercase();
            let (ox, oy) = if relative { (cx, cy) } else { (0.0, 0.0) };
            let upper = command.to_ascii_uppercase();
            let mut ctrl = None;
            match upper {
                b'M' => {
                    let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    (cx, cy) = (ox + x, oy + y);
                    (sx, sy) = (cx, cy);
                    sink.BeginFigure(point(cx, cy), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                    // Further pairs after a moveto are implicit linetos.
                    command = if relative { b'l' } else { b'L' };
                }
                b'L' => {
                    let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                    (cx, cy) = (ox + x, oy + y);
                    sink.AddLine(point(cx, cy));
                }
                b'H' => {
                    let Some(x) = t.number() else { break };
                    cx = ox + x;
                    sink.AddLine(point(cx, cy));
                }
                b'V' => {
                    let Some(y) = t.number() else { break };
                    cy = oy + y;
                    sink.AddLine(point(cx, cy));
                }
                b'C' | b'S' => {
                    let (x1, y1) = if upper == b'C' {
                        let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                        (ox + x, oy + y)
                    } else {
                        match last_ctrl {
                            Some((lx, ly, b'C')) => (2.0 * cx - lx, 2.0 * cy - ly),
                            _ => (cx, cy),
                        }
                    };
                    let (Some(x2), Some(y2), Some(x), Some(y)) = (t.number(), t.number(), t.number(), t.number()) else { break };
                    let (x2, y2, x, y) = (ox + x2, oy + y2, ox + x, oy + y);
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: point(x1, y1), point2: point(x2, y2), point3: point(x, y) });
                    ctrl = Some((x2, y2, b'C'));
                    (cx, cy) = (x, y);
                }
                b'Q' | b'T' => {
                    let (x1, y1) = if upper == b'Q' {
                        let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                        (ox + x, oy + y)
                    } else {
                        match last_ctrl {
                            Some((lx, ly, b'Q')) => (2.0 * cx - lx, 2.0 * cy - ly),
                            _ => (cx, cy),
                        }
                    };
                    let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                    let (x, y) = (ox + x, oy + y);
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT { point1: point(x1, y1), point2: point(x, y) });
                    ctrl = Some((x1, y1, b'Q'));
                    (cx, cy) = (x, y);
                }
                b'A' => {
                    let (Some(rx), Some(ry), Some(rotation)) = (t.number(), t.number(), t.number()) else { break };
                    let (Some(large), Some(sweep)) = (t.flag(), t.flag()) else { break };
                    let (Some(x), Some(y)) = (t.number(), t.number()) else { break };
                    (cx, cy) = (ox + x, oy + y);
                    sink.AddArc(&D2D1_ARC_SEGMENT {
                        point: point(cx, cy),
                        size: D2D_SIZE_F { width: rx.abs(), height: ry.abs() },
                        rotationAngle: rotation,
                        sweepDirection: if sweep { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                        arcSize: if large { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                    });
                }
                b'Z' => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    (cx, cy) = (sx, sy);
                    // A new subpath may start without "M" right after "z".
                    if t.has_number() {
                        sink.BeginFigure(point(cx, cy), D2D1_FIGURE_BEGIN_FILLED);
                        open = true;
                        command = if relative { b'l' } else { b'L' };
                    }
                }
                _ => break,
            }
            last_ctrl = ctrl;
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tokens;

    #[test]
    fn numbers_without_separators_are_split() {
        let mut t = Tokens { s: b".6.113-1.5e2,3", i: 0 };
        assert_eq!(t.number(), Some(0.6));
        assert_eq!(t.number(), Some(0.113));
        assert_eq!(t.number(), Some(-150.0));
        assert_eq!(t.number(), Some(3.0));
    }

    #[test]
    fn packed_arc_flags_are_read_one_by_one() {
        let mut t = Tokens { s: b"00-.626", i: 0 };
        assert_eq!(t.flag(), Some(false));
        assert_eq!(t.flag(), Some(false));
        assert_eq!(t.number(), Some(-0.626));
    }
}

#[cfg(test)]
mod preview {
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::{
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES, D2D1CreateFactory, ID2D1Factory,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
        WICBitmapCacheOnLoad,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
    use windows::core::Interface;
    use windows_numerics::Matrix3x2;

    // Renders the brand marks to the PNG named by RUSTPLAY_ICON_PREVIEW, to check the SVG parser by eye.
    #[test]
    #[ignore = "escribe una imagen; ejecutar a mano"]
    fn render_brand_marks() {
        let Ok(out) = std::env::var("RUSTPLAY_ICON_PREVIEW") else { return };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let bitmap = wic.CreateBitmap(3 * 140, 140, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                ..Default::default()
            };
            let target = factory.CreateWicBitmapRenderTarget(&bitmap, &props).unwrap();
            let brush = target.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.1, g: 0.1, b: 0.1, a: 1.0 }, None).unwrap();
            target.BeginDraw();
            target.Clear(Some(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }));
            for (i, data) in [crate::brands::GITHUB, crate::brands::BUY_ME_A_COFFEE, crate::brands::PAYPAL].iter().enumerate() {
                let geometry = super::geometry(&factory, data).unwrap();
                target.SetTransform(&(Matrix3x2::scale(5.0, 5.0) * Matrix3x2::translation(10.0 + i as f32 * 140.0, 10.0)));
                target.FillGeometry(&geometry, &brush, None);
            }
            target.EndDraw(None, None).unwrap();
            let stream = wic.CreateStream().unwrap();
            stream.InitializeFromFilename(&windows::core::HSTRING::from(out), 0x40000002).unwrap();
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()).unwrap();
            encoder.Initialize(&stream, windows::Win32::Graphics::Imaging::WICBitmapEncoderNoCache).unwrap();
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut::<Option<IPropertyBag2>>()).unwrap();
            let frame = frame.unwrap();
            frame.Initialize(None).unwrap();
            frame.WriteSource(&bitmap.cast::<windows::Win32::Graphics::Imaging::IWICBitmapSource>().unwrap(), std::ptr::null()).unwrap();
            frame.Commit().unwrap();
            encoder.Commit().unwrap();
        }
    }
}

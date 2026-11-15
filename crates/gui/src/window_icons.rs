use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits,
    GetObjectW, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetIconInfo, HICON, ICONINFO, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    WM_GETICON,
};

pub struct IconImage {
    pub width: u32,
    pub height: u32,
    // Premultiplied BGRA, top-down.
    pub pixels: Vec<u8>,
}

fn window_icon(hwnd: HWND) -> Option<HICON> {
    let ask = |kind: usize| {
        let mut result = 0usize;
        // SAFETY: mensaje de consulta con tiempo límite para no bloquear con ventanas colgadas.
        let sent = unsafe {
            SendMessageTimeoutW(hwnd, WM_GETICON, WPARAM(kind), LPARAM(0), SMTO_ABORTIFHUNG, 40, Some(&mut result))
        };
        (sent.0 != 0 && result != 0).then_some(HICON(result as *mut core::ffi::c_void))
    };
    // SAFETY: consulta de solo lectura de la clase de la ventana.
    let class = |index| {
        let raw = unsafe { GetClassLongPtrW(hwnd, index) };
        (raw != 0).then_some(HICON(raw as *mut core::ffi::c_void))
    };
    // ICON_SMALL2, ICON_SMALL, ICON_BIG, then the class icons.
    ask(2).or_else(|| ask(0)).or_else(|| ask(1)).or_else(|| class(GCLP_HICONSM)).or_else(|| class(GCLP_HICON))
}

fn read_bitmap(bitmap: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: búfer del tamaño exacto de la imagen y contexto de dispositivo temporal.
    let lines = unsafe {
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        lines
    };
    (lines == height).then_some(pixels)
}

// The window's small icon as premultiplied BGRA pixels, or None when it has no usable one.
pub fn icon_of(hwnd: isize) -> Option<IconImage> {
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let icon = window_icon(hwnd)?;
    let mut info = ICONINFO::default();
    // SAFETY: el icono pertenece a la ventana; los mapas de bits devueltos se liberan abajo.
    unsafe { GetIconInfo(icon, &mut info).ok()? };
    let result = (|| {
        if info.hbmColor.is_invalid() {
            return None;
        }
        let mut bitmap = BITMAP::default();
        // SAFETY: estructura de salida local del tamaño correcto.
        let got = unsafe { GetObjectW(info.hbmColor.into(), size_of::<BITMAP>() as i32, Some((&raw mut bitmap).cast())) };
        let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
        if got == 0 || width <= 0 || height <= 0 || width > 256 || height > 256 {
            return None;
        }
        let mut pixels = read_bitmap(info.hbmColor, width, height)?;
        if pixels.chunks_exact(4).all(|p| p[3] == 0) {
            // Icons without an alpha channel take their shape from the mask (white = transparent).
            let mask = if info.hbmMask.is_invalid() { None } else { read_bitmap(info.hbmMask, width, height) };
            for (i, p) in pixels.chunks_exact_mut(4).enumerate() {
                let transparent = mask.as_ref().is_some_and(|m| m[i * 4] != 0);
                p[3] = if transparent { 0 } else { 255 };
            }
        }
        for p in pixels.chunks_exact_mut(4) {
            let a = u16::from(p[3]);
            for c in &mut p[..3] {
                *c = (u16::from(*c) * a / 255) as u8;
            }
        }
        Some(IconImage { width: width as u32, height: height as u32, pixels })
    })();
    // SAFETY: mapas de bits creados por GetIconInfo, que el llamador debe destruir.
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask.into());
        }
    }
    result
}

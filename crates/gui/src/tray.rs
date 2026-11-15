use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Factory,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWriteCreateFactory,
    IDWriteFactory,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP, HGDIOBJ,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory, WICBitmapCacheOnLoad,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCSTR, PCWSTR, w};

use crate::i18n::{t, tf};

pub const WM_TRAY: u32 = WM_APP + 2;
pub const WM_SHOW_APP: u32 = WM_APP + 3;

pub const CMD_OPEN: u32 = 1;
pub const CMD_RECEIVER: u32 = 2;
pub const CMD_EXIT: u32 = 3;
pub const CMD_SETTINGS: u32 = 4;
pub const CMD_STOP_MIRROR: u32 = 5;
// Mirroring to device `i` of the menu model is CMD_DEVICE + i.
pub const CMD_DEVICE: u32 = 100;

const GLYPH_OPEN: char = '\u{E737}';
const GLYPH_CAST: char = '\u{EC15}';
const GLYPH_STOP: char = '\u{E71A}';
const GLYPH_SETTINGS: char = '\u{E713}';
const GLYPH_EXIT: char = '\u{E7E8}';

// What the context menu shows; built by the app each time the menu opens.
pub struct TrayMenu {
    pub status: String,
    pub dark: bool,
    pub receiver_on: bool,
    pub receiver_busy: bool,
    // (name, is the current mirroring target)
    pub devices: Vec<(String, bool)>,
    // Favourites on the network: (index into the menu's devices, name, is the current target).
    pub favorites: Vec<(usize, String, bool)>,
    pub mirroring: bool,
    pub can_mirror: bool,
}

fn data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn copy_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    nid.szTip = [0; 128];
    for (dst, src) in nid.szTip.iter_mut().take(127).zip(text.encode_utf16()) {
        *dst = src;
    }
}

pub fn add(hwnd: HWND) {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    unsafe {
        if let Ok(instance) = GetModuleHandleW(None) {
            let size = GetSystemMetrics(SM_CXSMICON);
            if let Ok(icon) = LoadImageW(Some(instance.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
                nid.hIcon = HICON(icon.0);
            }
        }
    }
    copy_tip(&mut nid, "RustPlay");
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

// Updates the tooltip shown when hovering the icon next to the clock.
pub fn set_tip(hwnd: HWND, text: &str) {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_TIP;
    copy_tip(&mut nid, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

// Notification balloon from the icon (Windows 10 and 11 show it as a notification); clicking it
// sends NIN_BALLOONUSERCLICK to WM_TRAY.
pub fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut nid = data(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
    for (dst, src) in nid.szInfoTitle.iter_mut().take(63).zip(title.encode_utf16()) {
        *dst = src;
    }
    // An empty text removes the balloon instead of showing it.
    let text = if text.is_empty() { " " } else { text };
    for (dst, src) in nid.szInfo.iter_mut().take(255).zip(text.encode_utf16()) {
        *dst = src;
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd));
    }
}

// uxtheme's undocumented SetPreferredAppMode (ordinal 135) and FlushMenuThemes (136) make Win32
// popup menus dark or light. Mode 1 follows Windows, 2 forces dark, 3 forces light.
fn set_menu_mode(mode: i32) {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(proc) = GetProcAddress(uxtheme, PCSTR(135 as _)) {
            let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(proc);
            set_preferred_app_mode(mode);
        }
        if let Some(proc) = GetProcAddress(uxtheme, PCSTR(136 as _)) {
            let flush_menu_themes: extern "system" fn() = std::mem::transmute(proc);
            flush_menu_themes();
        }
    }
}

pub fn allow_dark_menus() {
    set_menu_mode(1);
}

// Renders a Segoe Fluent Icons glyph into a premultiplied 32-bit DIB for a menu item.
struct Glyphs {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    wic: IWICImagingFactory,
    size: u32,
    color: D2D1_COLOR_F,
    created: Vec<HBITMAP>,
}

impl Glyphs {
    fn new(size: u32, dark: bool) -> Option<Self> {
        unsafe {
            let v = if dark { 1.0 } else { 0.0 };
            Some(Self {
                d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?,
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?,
                wic: CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?,
                size,
                color: D2D1_COLOR_F { r: v, g: v, b: v, a: 1.0 },
                created: Vec::new(),
            })
        }
    }

    fn bitmap(&mut self, glyph: char) -> Option<HBITMAP> {
        let size = self.size;
        unsafe {
            let wic_bitmap = self.wic.CreateBitmap(size, size, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).ok()?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                ..Default::default()
            };
            let target = self.d2d.CreateWicBitmapRenderTarget(&wic_bitmap, &props).ok()?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let format = self
                .dwrite
                .CreateTextFormat(
                    w!("Segoe Fluent Icons"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size as f32,
                    w!(""),
                )
                .ok()?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER).ok()?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER).ok()?;
            let brush = target.CreateSolidColorBrush(&self.color, None).ok()?;
            let mut buf = [0u16; 2];
            let text = glyph.encode_utf16(&mut buf);
            let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: size as f32, bottom: size as f32 };
            target.BeginDraw();
            target.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            target.DrawText(text, &format, &rect, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
            target.EndDraw(None, None).ok()?;

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size as i32,
                    biHeight: -(size as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let stride = size * 4;
            let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (stride * size) as usize);
            if wic_bitmap.CopyPixels(std::ptr::null(), stride, pixels).is_err() {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                return None;
            }
            self.created.push(bitmap);
            Some(bitmap)
        }
    }
}

impl Drop for Glyphs {
    fn drop(&mut self) {
        for bitmap in self.created.drain(..) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// Appends a text item; the strings must outlive the call only (the menu copies them).
unsafe fn item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str, icon: Option<HBITMAP>) {
    let text = wide(text);
    unsafe {
        let _ = AppendMenuW(menu, flags | MF_STRING, id, PCWSTR(text.as_ptr()));
        if let Some(bitmap) = icon {
            let info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_BITMAP,
                hbmpItem: bitmap,
                ..Default::default()
            };
            let _ = SetMenuItemInfoW(menu, id as u32, false, &info);
        }
    }
}

// Shows the context menu at the cursor and returns the chosen command, if any.
pub fn menu(hwnd: HWND, model: &TrayMenu) -> Option<u32> {
    set_menu_mode(if model.dark { 2 } else { 3 });
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let mut glyphs = Glyphs::new((16.0 * scale).round() as u32, model.dark);
    let mut icon = |g: char| glyphs.as_mut().and_then(|glyphs| glyphs.bitmap(g));
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        // Items show either their check mark or their icon, in the same column.
        let style = MENUINFO { cbSize: size_of::<MENUINFO>() as u32, fMask: MIM_STYLE, dwStyle: MNS_CHECKORBMP, ..Default::default() };
        let _ = SetMenuInfo(menu, &style);

        item(menu, MF_GRAYED, 0, &format!("RustPlay  ·  {}", model.status), None);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, MF_ENABLED, CMD_OPEN as usize, t("tray.open"), icon(GLYPH_OPEN));
        let _ = SetMenuDefaultItem(menu, CMD_OPEN, 0);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        for (index, name, current) in &model.favorites {
            let mut flags = if *current { MF_CHECKED } else { MF_UNCHECKED };
            if !model.can_mirror && !current {
                flags |= MF_GRAYED;
            }
            let label = tf("tray.send_to", &[("name", name)]);
            item(menu, flags, CMD_DEVICE as usize + index, &label, if *current { None } else { icon(GLYPH_CAST) });
        }
        if !model.favorites.is_empty() {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let devices = CreatePopupMenu().ok()?;
        if model.devices.is_empty() {
            item(devices, MF_GRAYED, 0, t("tray.no_devices"), None);
        }
        for (i, (name, current)) in model.devices.iter().enumerate() {
            let mut flags = if *current { MF_CHECKED } else { MF_UNCHECKED };
            if !model.can_mirror && !current {
                flags |= MF_GRAYED;
            }
            item(devices, flags, CMD_DEVICE as usize + i, name, None);
        }
        let cast = wide(t("tray.mirror_to"));
        let _ = AppendMenuW(menu, MF_POPUP | MF_STRING, devices.0 as usize, PCWSTR(cast.as_ptr()));
        if let Some(bitmap) = icon(GLYPH_CAST) {
            let info = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32, fMask: MIIM_BITMAP, hbmpItem: bitmap, ..Default::default() };
            let _ = SetMenuItemInfoW(menu, GetMenuItemCount(Some(menu)) as u32 - 1, true, &info);
        }
        if model.mirroring {
            item(menu, MF_ENABLED, CMD_STOP_MIRROR as usize, t("tray.stop_mirror"), icon(GLYPH_STOP));
        }
        let mut flags = if model.receiver_on { MF_CHECKED } else { MF_UNCHECKED };
        if model.receiver_busy {
            flags |= MF_GRAYED;
        }
        item(menu, flags, CMD_RECEIVER as usize, t("tray.receive"), None);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, MF_ENABLED, CMD_SETTINGS as usize, t("tray.settings"), icon(GLYPH_SETTINGS));
        item(menu, MF_ENABLED, CMD_EXIT as usize, t("tray.exit"), icon(GLYPH_EXIT));

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        drop(glyphs);
        (chosen.0 > 0).then_some(chosen.0 as u32)
    }
}

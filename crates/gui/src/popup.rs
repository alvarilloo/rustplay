// Small always-on-top prompts (PIN code, "allow this device?") drawn with GDI. Each lives on its
// own thread so it never blocks the main window, and never takes the keyboard focus.

use std::sync::Mutex;
use std::sync::mpsc;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLIP_DEFAULT_PRECIS, CreateFontW, CreatePen, CreateSolidBrush, DEFAULT_CHARSET,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint,
    FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, FillRect, GetStockObject, HFONT, HGDIOBJ, InvalidateRect, NULL_BRUSH,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, RoundRect, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    CLEARTYPE_QUALITY, DT_NOPREFIX,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

pub const WM_POPUP: u32 = WM_APP + 20;
const WM_POPUP_CLOSE: u32 = WM_APP + 21;
const TIMER_EXPIRE: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn colorref(self) -> COLORREF {
        COLORREF(u32::from(self.0) | u32::from(self.1) << 8 | u32::from(self.2) << 16)
    }
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub background: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub text_secondary: Rgb,
    pub button: Rgb,
    pub button_hover: Rgb,
    pub accent: Rgb,
    pub accent_hover: Rgb,
    pub text_on_accent: Rgb,
}

#[derive(Clone, Debug)]
pub struct AskButtons {
    pub allow: String,
    pub deny: String,
    pub remember: String,
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub title: String,
    pub body: String,
    // Big digits (the PIN code).
    pub code: Option<String>,
    pub buttons: Option<AskButtons>,
    pub palette: Palette,
    pub expire_secs: u32,
    // How many prompts are already on screen, so this one stacks above them.
    pub slot: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub allow: bool,
    pub remember: bool,
}

// Answers waiting for the main window: (tag given to `show`, outcome).
static RESULTS: Mutex<Vec<(u64, Outcome)>> = Mutex::new(Vec::new());

pub fn take_results() -> Vec<(u64, Outcome)> {
    std::mem::take(&mut *RESULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

// Closes a prompt from the outside, without reporting an answer.
pub fn close(handle: isize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(handle as _)), WM_POPUP_CLOSE, WPARAM(0), LPARAM(0));
    }
}

struct State {
    spec: Spec,
    owner: isize,
    tag: u64,
    remember: bool,
    hover: Option<Hit>,
    layout: Layout,
    fonts: Option<Fonts>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Allow,
    Deny,
    Remember,
}

#[derive(Default, Clone, Copy)]
struct Layout {
    scale: f32,
    allow: RECT,
    deny: RECT,
    remember: RECT,
}

struct Fonts {
    title: HFONT,
    body: HFONT,
    code: HFONT,
    icon: HFONT,
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in [self.title, self.body, self.code, self.icon] {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
        }
    }
}

const WIDTH: f32 = 380.0;
const MARGIN: f32 = 16.0;

fn height(spec: &Spec) -> f32 {
    if spec.buttons.is_some() { 188.0 } else { 196.0 }
}

// Shows a prompt; the handle closes it with `close`. Button clicks arrive through `take_results`
// after `WM_POPUP` is posted to `owner`.
pub fn show(owner: HWND, tag: u64, spec: Spec) -> Option<isize> {
    let (tx, rx) = mpsc::channel();
    let owner = owner.0 as isize;
    std::thread::Builder::new()
        .name("rustplay-popup".into())
        .spawn(move || run(owner, tag, spec, &tx))
        .ok()?;
    rx.recv().ok().flatten()
}

fn run(owner: isize, tag: u64, spec: Spec, ready: &mpsc::Sender<Option<isize>>) {
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            let _ = ready.send(None);
            return;
        };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("RustPlayPopup"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let expire = spec.expire_secs;
        let (width, h) = (WIDTH, height(&spec));
        let slot = spec.slot;
        let state = Box::new(State {
            spec,
            owner,
            tag,
            remember: false,
            hover: None,
            layout: Layout::default(),
            fonts: None,
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("RustPlayPopup"),
            w!("RustPlay"),
            WS_POPUP,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            Some(Box::into_raw(state).cast()),
        );
        let Ok(hwnd) = hwnd else {
            let _ = ready.send(None);
            return;
        };
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let (pw, ph) = ((width * scale) as i32, (h * scale) as i32);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some((&raw mut work).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let margin = (MARGIN * scale) as i32;
        let x = work.right - pw - margin;
        let y = work.bottom - ph - margin - slot as i32 * (ph + margin);
        let corners = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, (&raw const corners).cast(), size_of_val(&corners) as u32);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, pw, ph, SWP_NOACTIVATE);
        if expire > 0 {
            SetTimer(Some(hwnd), TIMER_EXPIRE, expire * 1000, None);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = ready.send(Some(hwnd.0 as isize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn state<'a>(hwnd: HWND) -> Option<&'a mut State> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    unsafe { ptr.as_mut() }
}

fn rect(x: f32, y: f32, w: f32, h: f32, scale: f32) -> RECT {
    RECT {
        left: (x * scale) as i32,
        top: (y * scale) as i32,
        right: ((x + w) * scale) as i32,
        bottom: ((y + h) * scale) as i32,
    }
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn compute_layout(spec: &Spec, scale: f32) -> Layout {
    let mut layout = Layout { scale, ..Default::default() };
    if spec.buttons.is_some() {
        let h = height(spec);
        let by = h - MARGIN - 32.0;
        let bw = (WIDTH - 2.0 * MARGIN - 8.0) / 2.0;
        layout.allow = rect(MARGIN, by, bw, 32.0, scale);
        layout.deny = rect(MARGIN + bw + 8.0, by, bw, 32.0, scale);
        layout.remember = rect(MARGIN, by - 36.0, WIDTH - 2.0 * MARGIN, 28.0, scale);
    }
    layout
}

fn make_font(height_px: f32, weight: i32, face: PCWSTR) -> HFONT {
    unsafe {
        CreateFontW(
            -(height_px as i32),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            FF_DONTCARE.0 as u32,
            face,
        )
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn draw_text(hdc: windows::Win32::Graphics::Gdi::HDC, text: &str, mut area: RECT, flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT) {
    let mut buffer = wide(text);
    unsafe {
        DrawTextW(hdc, &mut buffer, &mut area, flags | DT_NOPREFIX);
    }
}

fn paint(hwnd: HWND, st: &mut State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let scale = st.layout.scale.max(1.0);
        let fonts = st.fonts.get_or_insert_with(|| Fonts {
            title: make_font(16.0 * scale, FW_SEMIBOLD.0 as i32, w!("Segoe UI Variable Text")),
            body: make_font(13.0 * scale, FW_NORMAL.0 as i32, w!("Segoe UI Variable Text")),
            code: make_font(44.0 * scale, FW_SEMIBOLD.0 as i32, w!("Segoe UI Variable Display")),
            icon: make_font(14.0 * scale, FW_NORMAL.0 as i32, w!("Segoe Fluent Icons")),
        });
        let p = &st.spec.palette;
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let background = CreateSolidBrush(p.background.colorref());
        FillRect(hdc, &client, background);
        let _ = DeleteObject(HGDIOBJ(background.0));
        SetBkMode(hdc, TRANSPARENT);

        let title_area = rect(MARGIN, MARGIN, WIDTH - 2.0 * MARGIN, 24.0, scale);
        SelectObject(hdc, HGDIOBJ(fonts.title.0));
        SetTextColor(hdc, p.text.colorref());
        draw_text(hdc, &st.spec.title, title_area, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

        SelectObject(hdc, HGDIOBJ(fonts.body.0));
        SetTextColor(hdc, p.text_secondary.colorref());
        let body_h = if st.spec.code.is_some() { 20.0 } else { 40.0 };
        let body_area = rect(MARGIN, MARGIN + 30.0, WIDTH - 2.0 * MARGIN, body_h, scale);
        draw_text(hdc, &st.spec.body, body_area, DT_LEFT | DT_WORDBREAK);

        if let Some(code) = &st.spec.code {
            let spaced: String = code.chars().flat_map(|c| [c, '\u{2009}', '\u{2009}']).collect();
            SelectObject(hdc, HGDIOBJ(fonts.code.0));
            SetTextColor(hdc, p.accent.colorref());
            let area = rect(MARGIN, 86.0, WIDTH - 2.0 * MARGIN, 80.0, scale);
            draw_text(hdc, spaced.trim_end(), area, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        }

        if let Some(buttons) = st.spec.buttons.clone() {
            let layout = st.layout;
            let hover = st.hover;
            // Checkbox
            let box_size = (18.0 * scale) as i32;
            let cy = (layout.remember.top + layout.remember.bottom) / 2;
            let boxr = RECT { left: layout.remember.left, top: cy - box_size / 2, right: layout.remember.left + box_size, bottom: cy + box_size / 2 };
            let radius = (4.0 * scale) as i32;
            let pen = CreatePen(PS_SOLID, 1, if st.remember { p.accent } else { p.text_secondary }.colorref());
            let fill = CreateSolidBrush(if st.remember { p.accent } else { p.background }.colorref());
            let (old_pen, old_brush) = (SelectObject(hdc, HGDIOBJ(pen.0)), SelectObject(hdc, HGDIOBJ(fill.0)));
            let _ = RoundRect(hdc, boxr.left, boxr.top, boxr.right, boxr.bottom, radius, radius);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let _ = DeleteObject(HGDIOBJ(fill.0));
            if st.remember {
                SelectObject(hdc, HGDIOBJ(fonts.icon.0));
                SetTextColor(hdc, p.text_on_accent.colorref());
                draw_text(hdc, "\u{E73E}", boxr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
            }
            SelectObject(hdc, HGDIOBJ(fonts.body.0));
            SetTextColor(hdc, p.text.colorref());
            let label = RECT { left: boxr.right + (8.0 * scale) as i32, ..layout.remember };
            draw_text(hdc, &buttons.remember, label, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);

            for (hit, area, text, primary) in [(Hit::Allow, layout.allow, &buttons.allow, true), (Hit::Deny, layout.deny, &buttons.deny, false)] {
                let hot = hover == Some(hit);
                let (fill, edge) = match (primary, hot) {
                    (true, false) => (p.accent, p.accent),
                    (true, true) => (p.accent_hover, p.accent_hover),
                    (false, false) => (p.button, p.border),
                    (false, true) => (p.button_hover, p.border),
                };
                let pen = CreatePen(PS_SOLID, 1, edge.colorref());
                let brush = CreateSolidBrush(fill.colorref());
                let (old_pen, old_brush) = (SelectObject(hdc, HGDIOBJ(pen.0)), SelectObject(hdc, HGDIOBJ(brush.0)));
                let r = (4.0 * scale) as i32 * 2;
                let _ = RoundRect(hdc, area.left, area.top, area.right, area.bottom, r, r);
                SelectObject(hdc, old_pen);
                SelectObject(hdc, old_brush);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                let _ = DeleteObject(HGDIOBJ(brush.0));
                SelectObject(hdc, HGDIOBJ(fonts.body.0));
                SetTextColor(hdc, if primary { p.text_on_accent } else { p.text }.colorref());
                draw_text(hdc, text, area, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            }
        }

        // Hairline border
        let border = CreatePen(PS_SOLID, 1, p.border.colorref());
        let (old_pen, old_brush) = (SelectObject(hdc, HGDIOBJ(border.0)), SelectObject(hdc, GetStockObject(NULL_BRUSH)));
        let corner = (8.0 * scale) as i32 * 2;
        let _ = RoundRect(hdc, client.left, client.top, client.right, client.bottom, corner, corner);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(border.0));
        let _ = EndPaint(hwnd, &ps);
    }
}

fn finish(hwnd: HWND, st: &State, allow: bool) {
    RESULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((st.tag, Outcome { allow, remember: st.remember }));
    unsafe {
        let _ = PostMessageW(Some(HWND(st.owner as _)), WM_POPUP, WPARAM(0), LPARAM(0));
        let _ = DestroyWindow(hwnd);
    }
}

fn hit_test(st: &State, x: i32, y: i32) -> Option<Hit> {
    st.spec.buttons.as_ref()?;
    let l = &st.layout;
    if contains(&l.allow, x, y) {
        Some(Hit::Allow)
    } else if contains(&l.deny, x, y) {
        Some(Hit::Deny)
    } else if contains(&l.remember, x, y) {
        Some(Hit::Remember)
    } else {
        None
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        unsafe {
            let create = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let Some(st) = state(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let point = |lparam: LPARAM| (i32::from(lparam.0 as i16), i32::from((lparam.0 >> 16) as i16));
    match msg {
        WM_CREATE => {
            let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
            st.layout = compute_layout(&st.spec, scale);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = point(lparam);
            let hover = hit_test(st, x, y);
            if hover != st.hover {
                st.hover = hover;
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = point(lparam);
            match hit_test(st, x, y) {
                Some(Hit::Allow) => finish(hwnd, st, true),
                Some(Hit::Deny) => finish(hwnd, st, false),
                Some(Hit::Remember) => {
                    st.remember = !st.remember;
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                None => {}
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => unsafe {
            if st.hover.is_some() {
                SetCursor(LoadCursorW(None, IDC_HAND).ok());
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
        WM_TIMER | WM_POPUP_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(std::ptr::from_mut(st)));
                PostQuitMessage(0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

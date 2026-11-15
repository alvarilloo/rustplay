// Pure parts of the "cinema mode" of the mirror window: keyboard shortcuts, toasts, the borderless
// window's hit testing and the placement of the blurred ambient background.
use windows::Win32::Foundation::RECT;

// A blurred layer is rendered at this fraction of the window size and refreshed every N frames.
pub(super) const AMBIENT_DIVISOR: u32 = 8;
pub(super) const AMBIENT_MIN: u32 = 16;
pub(super) const AMBIENT_REFRESH_FRAMES: u32 = 4;
// Gaussian standard deviation in pixels of the low-resolution layer (about 32 px on screen).
pub(super) const AMBIENT_SIGMA: f32 = 4.0;
// How much of the blurred layer is covered with black.
pub(super) const AMBIENT_DIM: f32 = 0.38;

pub(super) const TOAST_HOLD_MS: u64 = 1500;
pub(super) const TOAST_FADE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shortcut {
    Fullscreen,
    ExitFullscreen,
    Record,
    Screenshot,
    Mute,
    Blur,
    Cinema,
    Lock,
    Stats,
    TopMost,
}

// What a toast talks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToastKind {
    Fullscreen = 1,
    Record,
    Screenshot,
    Mute,
    Blur,
    Cinema,
    Lock,
    Stats,
    TopMost,
}

const KINDS: [ToastKind; 9] = [
    ToastKind::Fullscreen,
    ToastKind::Record,
    ToastKind::Screenshot,
    ToastKind::Mute,
    ToastKind::Blur,
    ToastKind::Cinema,
    ToastKind::Lock,
    ToastKind::Stats,
    ToastKind::TopMost,
];

// Maps a virtual-key code (and whether Ctrl or Alt is held) to a shortcut.
pub(super) fn shortcut_for_key(vk: usize, fullscreen: bool, ctrl_or_alt: bool) -> Option<Shortcut> {
    if ctrl_or_alt {
        return None;
    }
    match vk {
        0x46 | 0x7A => Some(Shortcut::Fullscreen),
        0x1B if fullscreen => Some(Shortcut::ExitFullscreen),
        0x52 => Some(Shortcut::Record),
        0x53 => Some(Shortcut::Screenshot),
        0x4D => Some(Shortcut::Mute),
        0x42 => Some(Shortcut::Blur),
        0x43 => Some(Shortcut::Cinema),
        0x4C => Some(Shortcut::Lock),
        0x49 => Some(Shortcut::Stats),
        0x54 => Some(Shortcut::TopMost),
        _ => None,
    }
}

// A toast packed into one non-zero word: kind in the upper bits, state in the lower two
// (0 = no state, 1 = off, 2 = on).
pub(super) fn encode_toast(kind: ToastKind, state: Option<bool>) -> u32 {
    let s = match state {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    ((kind as u32) << 2) | s
}

pub(super) fn decode_toast(code: u32) -> Option<(ToastKind, Option<bool>)> {
    let kind = *KINDS.get((code >> 2).checked_sub(1)? as usize)?;
    let state = match code & 3 {
        0 => None,
        1 => Some(false),
        _ => Some(true),
    };
    Some((kind, state))
}

// 1 while the toast is held, then fades to 0; None once it is over.
pub(super) fn toast_alpha(elapsed_ms: u64) -> Option<f32> {
    if elapsed_ms < TOAST_HOLD_MS {
        Some(1.0)
    } else if elapsed_ms < TOAST_HOLD_MS + TOAST_FADE_MS {
        Some(1.0 - (elapsed_ms - TOAST_HOLD_MS) as f32 / TOAST_FADE_MS as f32)
    } else {
        None
    }
}

// "Label: on" (or just the label when the action has no state).
pub(super) fn toast_text(label: &str, state: Option<bool>, on: &str, off: &str) -> String {
    match state {
        None => label.to_owned(),
        Some(true) => format!("{label}: {on}"),
        Some(false) => format!("{label}: {off}"),
    }
}

// Size of the low-resolution layer for a window of `w`×`h`.
pub(super) fn ambient_size(w: u32, h: u32) -> (u32, u32) {
    ((w / AMBIENT_DIVISOR).max(AMBIENT_MIN), (h / AMBIENT_DIVISOR).max(AMBIENT_MIN))
}

// Whether the layer is refreshed on the `n`th presented frame.
pub(super) fn ambient_refresh_due(n: u32) -> bool {
    n % AMBIENT_REFRESH_FRAMES == 0
}

// The two strips of the window the picture does not cover (left/right or top/bottom).
pub(super) fn bars(picture: RECT, (w, h): (u32, u32)) -> [Option<RECT>; 2] {
    let (w, h) = (w as i32, h as i32);
    let part = |left, top, right, bottom| (right > left && bottom > top).then_some(RECT { left, top, right, bottom });
    [
        part(0, 0, picture.left.max(0), h).or_else(|| part(0, 0, w, picture.top.max(0))),
        part(picture.right.min(w), 0, w, h).or_else(|| part(0, picture.bottom.min(h), w, h)),
    ]
}

// Source crop of the frame and destination (the whole low-resolution layer) of the ambient blit.
pub(super) fn ambient_placement(crop: RECT, (aw, ah): (u32, u32)) -> (RECT, RECT) {
    (crop, RECT { left: 0, top: 0, right: aw as i32, bottom: ah as i32 })
}

// Scale that stretches the low-resolution layer over the whole window.
pub(super) fn ambient_scale((aw, ah): (u32, u32), (w, h): (u32, u32)) -> (f32, f32) {
    (w as f32 / aw.max(1) as f32, h as f32 / ah.max(1) as f32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Zone {
    Inside,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

// Which part of a borderless window of `w`×`h` the point (x, y) is in; the outer `border`
// pixels resize it.
pub(super) fn zone(x: i32, y: i32, w: i32, h: i32, border: i32) -> Zone {
    let (l, r) = (x < border, x >= w - border);
    let (t, b) = (y < border, y >= h - border);
    match (l, r, t, b) {
        (true, _, true, _) => Zone::TopLeft,
        (_, true, true, _) => Zone::TopRight,
        (true, _, _, true) => Zone::BottomLeft,
        (_, true, _, true) => Zone::BottomRight,
        (true, ..) => Zone::Left,
        (_, true, ..) => Zone::Right,
        (_, _, true, _) => Zone::Top,
        (_, _, _, true) => Zone::Bottom,
        _ => Zone::Inside,
    }
}

const WS_POPUP: u32 = 0x8000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const WS_MINIMIZEBOX: u32 = 0x0002_0000;
const WS_MAXIMIZEBOX: u32 = 0x0001_0000;
const WS_OVERLAPPEDWINDOW: u32 = WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;

// The window style with the frame of a normal window or the borderless one (which keeps the
// sizing frame so Windows still draws the shadow and lets the edges resize).
pub(super) fn swap_style(style: u32, borderless: bool) -> u32 {
    let base = style & !(WS_OVERLAPPEDWINDOW | WS_POPUP);
    if borderless {
        base | WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX
    } else {
        base | WS_OVERLAPPEDWINDOW
    }
}

// Hit-test codes of the framed window while its position is locked: moving, sizing and
// maximising do nothing, the rest (close, minimise) keeps working.
pub(super) fn locked_hit(code: u32) -> u32 {
    const HTBORDER: u32 = 18;
    match code {
        2 | 4 | 9 | 10..=17 => HTBORDER,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    fn tuple(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    #[test]
    fn shortcuts_map_keys() {
        assert_eq!(shortcut_for_key(0x46, false, false), Some(Shortcut::Fullscreen));
        assert_eq!(shortcut_for_key(0x7A, true, false), Some(Shortcut::Fullscreen));
        assert_eq!(shortcut_for_key(0x1B, true, false), Some(Shortcut::ExitFullscreen));
        assert_eq!(shortcut_for_key(0x1B, false, false), None);
        let keys = [
            (b'R', Shortcut::Record),
            (b'S', Shortcut::Screenshot),
            (b'M', Shortcut::Mute),
            (b'B', Shortcut::Blur),
            (b'C', Shortcut::Cinema),
            (b'L', Shortcut::Lock),
            (b'I', Shortcut::Stats),
            (b'T', Shortcut::TopMost),
        ];
        for (key, shortcut) in keys {
            assert_eq!(shortcut_for_key(usize::from(key), false, false), Some(shortcut));
            assert_eq!(shortcut_for_key(usize::from(key), false, true), None, "Ctrl/Alt combos are not ours");
        }
        assert_eq!(shortcut_for_key(usize::from(b'Z'), false, false), None);
    }

    #[test]
    fn toasts_round_trip_and_fade() {
        for kind in KINDS {
            for state in [None, Some(false), Some(true)] {
                let code = encode_toast(kind, state);
                assert_ne!(code, 0);
                assert_eq!(decode_toast(code), Some((kind, state)));
            }
        }
        assert_eq!(decode_toast(0), None);
        assert_eq!(decode_toast(99 << 2), None);
        assert_eq!(toast_alpha(0), Some(1.0));
        assert_eq!(toast_alpha(1499), Some(1.0));
        let mid = toast_alpha(1500 + 125).expect("fading");
        assert!((mid - 0.5).abs() < 1e-3);
        assert_eq!(toast_alpha(1750), None);
        assert_eq!(toast_text("Fondo", Some(true), "sí", "no"), "Fondo: sí");
        assert_eq!(toast_text("Fondo", Some(false), "sí", "no"), "Fondo: no");
        assert_eq!(toast_text("Captura", None, "sí", "no"), "Captura");
    }

    #[test]
    fn ambient_layer_geometry() {
        assert_eq!(ambient_size(1920, 1080), (240, 135));
        assert_eq!(ambient_size(100, 50), (16, 16));
        let (src, dst) = ambient_placement(r(0, 500, 1000, 1500), (125, 125));
        assert_eq!(tuple(src), (0, 500, 1000, 1500));
        assert_eq!(tuple(dst), (0, 0, 125, 125));
        assert_eq!(ambient_scale((240, 135), (1920, 1080)), (8.0, 8.0));
        assert!(ambient_refresh_due(0) && ambient_refresh_due(4) && !ambient_refresh_due(1));
    }

    #[test]
    fn bars_are_what_the_picture_leaves() {
        // Portrait picture centred in a landscape window: bars left and right.
        let [a, b] = bars(r(656, 0, 1264, 1080), (1920, 1080));
        assert_eq!(a.map(tuple), Some((0, 0, 656, 1080)));
        assert_eq!(b.map(tuple), Some((1264, 0, 1920, 1080)));
        // Letterbox: bars above and below.
        let [a, b] = bars(r(0, 100, 1000, 900), (1000, 1000));
        assert_eq!(a.map(tuple), Some((0, 0, 1000, 100)));
        assert_eq!(b.map(tuple), Some((0, 900, 1000, 1000)));
        // The picture fills the window: nothing to paint.
        assert_eq!(bars(r(0, 0, 640, 360), (640, 360)), [None, None]);
    }

    #[test]
    fn borderless_zones() {
        assert_eq!(zone(500, 400, 1000, 800, 8), Zone::Inside);
        assert_eq!(zone(2, 400, 1000, 800, 8), Zone::Left);
        assert_eq!(zone(998, 400, 1000, 800, 8), Zone::Right);
        assert_eq!(zone(500, 1, 1000, 800, 8), Zone::Top);
        assert_eq!(zone(500, 799, 1000, 800, 8), Zone::Bottom);
        assert_eq!(zone(1, 1, 1000, 800, 8), Zone::TopLeft);
        assert_eq!(zone(999, 1, 1000, 800, 8), Zone::TopRight);
        assert_eq!(zone(1, 799, 1000, 800, 8), Zone::BottomLeft);
        assert_eq!(zone(999, 799, 1000, 800, 8), Zone::BottomRight);
    }

    #[test]
    fn styles_swap_between_framed_and_borderless() {
        let visible = 0x1000_0000;
        let framed = swap_style(visible, false);
        assert_eq!(framed & WS_OVERLAPPEDWINDOW, WS_OVERLAPPEDWINDOW);
        assert_eq!(framed & WS_POPUP, 0);
        let bare = swap_style(framed, true);
        assert_eq!(bare & WS_CAPTION, 0, "no title bar");
        assert_ne!(bare & WS_THICKFRAME, 0, "edges still resize");
        assert_ne!(bare & WS_POPUP, 0);
        assert_ne!(bare & visible, 0, "other bits are kept");
        assert_eq!(swap_style(bare, false), framed);
    }

    #[test]
    fn locked_windows_keep_close_and_minimise() {
        assert_eq!(locked_hit(2), 18, "title bar");
        assert_eq!(locked_hit(13), 18, "sizing edge");
        assert_eq!(locked_hit(9), 18, "maximise");
        assert_eq!(locked_hit(20), 20, "close");
        assert_eq!(locked_hit(8), 8, "minimise");
        assert_eq!(locked_hit(1), 1, "client");
    }
}

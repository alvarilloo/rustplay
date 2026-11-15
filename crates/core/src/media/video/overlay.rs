// Hover controls of the mirror window. The layout and hit-testing are shared by the window thread
// (mouse input) and the render thread (drawing with Direct2D on the swap chain's back buffer).
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::Vector2;

use crate::Result;

// The controls hide this long after the pointer stops moving.
pub(super) const HIDE_AFTER_MS: u64 = 2500;
pub(super) const FADE_SECONDS: f32 = 0.15;

const BUTTON: f32 = 40.0;
const GAP: f32 = 4.0;
const PAD: f32 = 6.0;
const MARGIN: f32 = 28.0;
// Width of the record button while it shows the elapsed time.
const RECORDING_WIDTH: f32 = 108.0;
const MAX_BUTTONS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Action {
    Fullscreen,
    OpenApp,
    Record,
    Screenshot,
    Stop,
}

pub(super) fn actions(open_app: bool, capture: bool) -> &'static [Action] {
    use Action::*;
    match (open_app, capture) {
        (false, false) => &[Fullscreen, Stop],
        (true, false) => &[Fullscreen, OpenApp, Stop],
        (false, true) => &[Fullscreen, Record, Screenshot, Stop],
        (true, true) => &[Fullscreen, OpenApp, Record, Screenshot, Stop],
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
}

pub(super) struct Layout {
    pub bar: Area,
    pub buttons: [Area; MAX_BUTTONS],
    pub count: usize,
}

// A pill of buttons centred near the bottom edge; sizes are in pixels for `scale` = DPI / 96.
// The record button is wider while recording, to show the elapsed time.
pub(super) fn layout(width: f32, height: f32, scale: f32, actions: &[Action], recording: bool) -> Layout {
    let count = actions.len().min(MAX_BUTTONS);
    let (b, g, p) = (BUTTON * scale, GAP * scale, PAD * scale);
    let mut widths = [b; MAX_BUTTONS];
    for (w, action) in widths.iter_mut().zip(actions) {
        if recording && *action == Action::Record {
            *w = RECORDING_WIDTH * scale;
        }
    }
    let w = widths[..count].iter().sum::<f32>() + count.saturating_sub(1) as f32 * g + 2.0 * p;
    let h = b + 2.0 * p;
    let bar = Area { x: ((width - w) / 2.0).round(), y: (height - MARGIN * scale - h).round(), w, h };
    let mut buttons = [Area::default(); MAX_BUTTONS];
    let mut x = bar.x + p;
    for (slot, bw) in buttons.iter_mut().zip(widths).take(count) {
        *slot = Area { x, y: bar.y + p, w: bw, h: b };
        x += bw + g;
    }
    Layout { bar, buttons, count }
}

impl Layout {
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.buttons[..self.count].iter().position(|a| a.contains(x, y))
    }

    pub fn over_bar(&self, x: f32, y: f32) -> bool {
        self.bar.contains(x, y)
    }
}

#[derive(Clone)]
pub(super) struct Fonts {
    pub scale: f32,
    pub icon: IDWriteTextFormat,
    pub label: IDWriteTextFormat,
}

pub(super) struct View<'a> {
    pub alpha: f32,
    pub hovered: Option<usize>,
    pub pressed: bool,
    pub fullscreen: bool,
    // Seconds recorded so far, or None when not recording.
    pub recording: Option<u64>,
    pub actions: &'a [Action],
    pub label: &'a dyn Fn(Action, bool, bool) -> &'a str,
    pub stats: Option<&'a str>,
    // Short message with its opacity, shown at the top centre.
    pub toast: Option<(&'a str, f32)>,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn glyph(action: Action, fullscreen: bool) -> &'static str {
    match action {
        Action::Fullscreen if fullscreen => "\u{E73F}",
        Action::Fullscreen => "\u{E740}",
        Action::OpenApp => "\u{E8A7}",
        Action::Screenshot => "\u{E722}",
        Action::Stop => "\u{E711}",
        Action::Record => "",
    }
}

// "3:07" or "1:02:03".
pub(super) fn format_elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

// Width of `text` laid out with `format`, in pixels.
unsafe fn text_width(dwrite: &IDWriteFactory, format: &IDWriteTextFormat, text: &[u16]) -> Result<f32> {
    // SAFETY: disposición de texto temporal sobre un formato válido.
    unsafe {
        let layout = dwrite.CreateTextLayout(text, format, 10_000.0, 1_000.0)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        Ok(metrics.widthIncludingTrailingWhitespace)
    }
}

// A dark rounded label with white text, centred horizontally on `cx` with its top at `top`
// (or its bottom at `top` when `above`).
#[allow(clippy::too_many_arguments)]
unsafe fn pill(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    format: &IDWriteTextFormat,
    text: &str,
    anchor: (f32, f32),
    centred: bool,
    above: bool,
    scale: f32,
    alpha: f32,
) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        let tw = text_width(dwrite, format, &wide)?;
        let (w, h) = (tw + 20.0 * scale, 28.0 * scale);
        let x = if centred { anchor.0 - w / 2.0 } else { anchor.0 };
        let y = if above { anchor.1 - h } else { anchor.1 };
        let area = Area { x: x.round(), y: y.round(), w, h };
        let rounded = D2D1_ROUNDED_RECT { rect: area.d2d(), radiusX: 5.0 * scale, radiusY: 5.0 * scale };
        let fill = dc.CreateSolidColorBrush(&color(0.13, 0.13, 0.13, 0.92 * alpha), None)?;
        let edge = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.10 * alpha), None)?;
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, alpha), None)?;
        dc.FillRoundedRectangle(&rounded, &fill);
        dc.DrawRoundedRectangle(&rounded, &edge, 1.0, None);
        dc.DrawText(&wide, format, &area.d2d(), &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }
    Ok(())
}

// Draws the statistics label and the control bar into the target bound to `dc`.
pub(super) unsafe fn draw(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    fonts: &Fonts,
    (width, height): (u32, u32),
    view: &View<'_>,
) -> Result<()> {
    let scale = fonts.scale;
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        if let Some(text) = view.stats {
            pill(dc, dwrite, &fonts.label, text, (12.0 * scale, 12.0 * scale), false, false, scale, 1.0)?;
        }
        if let Some((text, alpha)) = view.toast {
            pill(dc, dwrite, &fonts.label, text, (width as f32 / 2.0, 16.0 * scale), true, false, scale, alpha)?;
        }
        if view.alpha <= 0.0 {
            return Ok(());
        }
        let a = view.alpha;
        let layout = layout(width as f32, height as f32, scale, view.actions, view.recording.is_some());
        let bar = D2D1_ROUNDED_RECT { rect: layout.bar.d2d(), radiusX: 8.0 * scale, radiusY: 8.0 * scale };
        let fill = dc.CreateSolidColorBrush(&color(0.11, 0.11, 0.11, 0.86 * a), None)?;
        let edge = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.12 * a), None)?;
        dc.FillRoundedRectangle(&bar, &fill);
        dc.DrawRoundedRectangle(&bar, &edge, 1.0, None);
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, a), None)?;
        for (i, action) in view.actions.iter().enumerate().take(layout.count) {
            let area = layout.buttons[i];
            if view.hovered == Some(i) {
                let highlight = match (action, view.pressed) {
                    (Action::Stop, false) => color(0.77, 0.17, 0.11, 0.95 * a),
                    (Action::Stop, true) => color(0.64, 0.14, 0.09, 0.95 * a),
                    (_, false) => color(1.0, 1.0, 1.0, 0.10 * a),
                    (_, true) => color(1.0, 1.0, 1.0, 0.05 * a),
                };
                let brush = dc.CreateSolidColorBrush(&highlight, None)?;
                let rounded = D2D1_ROUNDED_RECT { rect: area.d2d(), radiusX: 5.0 * scale, radiusY: 5.0 * scale };
                dc.FillRoundedRectangle(&rounded, &brush);
            }
            if *action == Action::Record {
                let red = dc.CreateSolidColorBrush(&color(0.92, 0.2, 0.18, a), None)?;
                let radius = 7.0 * scale;
                let cy = area.y + area.h / 2.0;
                let cx = match view.recording {
                    Some(_) => area.x + 14.0 * scale + radius,
                    None => area.x + area.w / 2.0,
                };
                let dot = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
                dc.FillEllipse(&dot, &red);
                if let Some(secs) = view.recording {
                    let text: Vec<u16> = format_elapsed(secs).encode_utf16().collect();
                    let rect = D2D_RECT_F {
                        left: cx + radius + 8.0 * scale,
                        top: area.y,
                        right: area.x + area.w,
                        bottom: area.y + area.h,
                    };
                    dc.DrawText(&text, &fonts.label, &rect, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                }
                continue;
            }
            let icon: Vec<u16> = glyph(*action, view.fullscreen).encode_utf16().collect();
            dc.DrawText(&icon, &fonts.icon, &area.d2d(), &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
        if let Some(i) = view.hovered.filter(|i| *i < layout.count) {
            let area = layout.buttons[i];
            let text = (view.label)(view.actions[i], view.fullscreen, view.recording.is_some());
            if !text.is_empty() {
                let anchor = (area.x + area.w / 2.0, layout.bar.y - 8.0 * scale);
                pill(dc, dwrite, &fonts.label, text, anchor, true, true, scale, a)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_are_centred_at_the_bottom_and_hit_tested() {
        let three = [Action::Fullscreen, Action::OpenApp, Action::Stop];
        let l = layout(1000.0, 800.0, 1.0, &three, false);
        assert_eq!(l.bar.w, 3.0 * 40.0 + 2.0 * 4.0 + 12.0);
        assert_eq!(l.bar.x, ((1000.0 - l.bar.w) / 2.0f32).round());
        assert!(l.bar.y + l.bar.h <= 800.0 - 28.0);
        let first = l.buttons[0];
        assert_eq!(l.hit(first.x + 1.0, first.y + 1.0), Some(0));
        let last = l.buttons[2];
        assert_eq!(l.hit(last.x + last.w - 1.0, last.y + last.h - 1.0), Some(2));
        assert_eq!(l.hit(10.0, 10.0), None);
        assert!(l.over_bar(l.bar.x + 1.0, l.bar.y + 1.0));
        let two = layout(1000.0, 800.0, 2.0, &three[..2], false);
        assert_eq!(two.count, 2);
        assert_eq!(two.hit(two.buttons[1].x + 1.0, two.buttons[1].y + 1.0), Some(1));
        assert_eq!(two.hit(two.buttons[1].x + two.buttons[1].w + 1.0, two.buttons[1].y + 1.0), None);
    }

    #[test]
    fn open_app_button_is_optional() {
        assert_eq!(actions(false, false), &[Action::Fullscreen, Action::Stop]);
        assert_eq!(actions(true, false), &[Action::Fullscreen, Action::OpenApp, Action::Stop]);
    }

    #[test]
    fn capture_buttons_sit_before_stop() {
        assert_eq!(actions(false, true), &[Action::Fullscreen, Action::Record, Action::Screenshot, Action::Stop]);
        let all = actions(true, true);
        assert_eq!(all.len(), 5);
        assert_eq!(all[4], Action::Stop);
    }

    #[test]
    fn the_record_button_widens_while_recording() {
        let list = actions(true, true);
        let idle = layout(1200.0, 800.0, 1.0, list, false);
        let rec = layout(1200.0, 800.0, 1.0, list, true);
        assert_eq!(idle.count, 5);
        assert_eq!(idle.bar.w, 5.0 * 40.0 + 4.0 * 4.0 + 12.0);
        assert_eq!(rec.bar.w, idle.bar.w + (RECORDING_WIDTH - BUTTON));
        let record = rec.buttons[2];
        assert_eq!(record.w, RECORDING_WIDTH);
        // Buttons stay in order, with the gap between them, and the bar is centred.
        for pair in rec.buttons[..5].windows(2) {
            assert_eq!(pair[1].x, pair[0].x + pair[0].w + GAP);
        }
        assert_eq!(rec.hit(record.x + record.w - 1.0, record.y + 1.0), Some(2));
        assert_eq!(rec.hit(rec.buttons[3].x + 1.0, rec.buttons[3].y + 1.0), Some(3));
        assert_eq!(rec.hit(rec.bar.x + 1.0, rec.bar.y + 1.0), None, "padding is not a button");
        assert!(rec.over_bar(rec.bar.x + 1.0, rec.bar.y + 1.0));
        assert!(rec.buttons[4].x + rec.buttons[4].w + PAD <= rec.bar.x + rec.bar.w);
    }

    #[test]
    fn elapsed_time_is_formatted() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(187), "3:07");
        assert_eq!(format_elapsed(3723), "1:02:03");
    }
}

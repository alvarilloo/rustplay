use tracing::Level;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use super::widgets::Style;
use crate::i18n::{t, tf};
use super::{App, Id, TITLE_H, glyph};
use crate::gfx::{Align, Color, Font, Rect};

const CF_UNICODETEXT: u32 = 13;
const LINE_PAD: f32 = 6.0;
const TIME_W: f32 = 96.0;
const LEVEL_W: f32 = 64.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LogFilter {
    All,
    Warnings,
    Errors,
}

impl LogFilter {
    fn level(self) -> Level {
        match self {
            LogFilter::All => Level::TRACE,
            LogFilter::Warnings => Level::WARN,
            LogFilter::Errors => Level::ERROR,
        }
    }
}

fn level_label(level: Level) -> &'static str {
    match level {
        Level::ERROR => t("logs.level.error"),
        Level::WARN => t("logs.level.warn"),
        Level::INFO => t("logs.level.info"),
        Level::DEBUG => t("logs.level.debug"),
        Level::TRACE => t("logs.level.trace"),
    }
}

pub fn open_folder() {
    let Ok(dir) = rustplay_core::identity::data_dir() else { return };
    let wide: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

pub(super) fn set_clipboard(hwnd: windows::Win32::Foundation::HWND, text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = (|| {
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let ptr = GlobalLock(memory).cast::<u16>();
            if ptr.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(memory);
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).ok()
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

impl App {
    pub(super) fn copy_logs(&mut self) {
        let entries = crate::logbuf::buffer().snapshot(self.logs.filter.level());
        let mut text = String::with_capacity(entries.len() * 80);
        for e in &entries {
            text.push_str(&format!("{} {:<5} {}\r\n", e.time, level_label(e.level), e.text));
        }
        if set_clipboard(self.hwnd, &text) {
            self.toast(&tf("logs.copied", &[("count", &entries.len())]));
        }
    }

    fn level_color(&self, level: Level) -> Color {
        let th = self.theme;
        match level {
            Level::ERROR => th.critical,
            Level::WARN => th.caution,
            Level::INFO => th.accent,
            _ => th.text_tertiary,
        }
    }

    pub(super) fn draw_logs(&mut self, x: f32, w: f32, h: f32) {
        let th = self.theme;
        // Page actions sit on the header line, aligned to the right.
        let mut bx = x + w;
        for (id, label, icon) in [
            (Id::LogClear, t("logs.clear"), glyph::DELETE),
            (Id::LogFolder, t("logs.open_folder"), glyph::FOLDER),
            (Id::LogCopy, t("logs.copy"), glyph::COPY),
        ] {
            let bw = self.button_width(label, Some(icon));
            bx -= bw;
            self.button(id, Rect::new(bx, TITLE_H + 30.0, bw, 32.0), label, Some(icon), Style::Standard, true);
            bx -= 8.0;
        }

        let y = TITLE_H + 104.0;
        let filters = [
            (LogFilter::All, t("logs.filter.all")),
            (LogFilter::Warnings, t("logs.filter.warnings")),
            (LogFilter::Errors, t("logs.filter.errors")),
        ];
        let mut sx = x;
        let seg = Rect::new(x, y, 0.0, 32.0);
        let widths: Vec<f32> = filters.iter().map(|(_, l)| self.gfx.measure(l, Font::Body, 400.0, false).0 + 28.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 8.0;
        let seg = Rect::new(seg.x, seg.y, total, seg.h);
        self.gfx.fill(seg, 6.0, th.control_pressed);
        self.gfx.stroke(seg, 6.0, 1.0, th.control_stroke);
        sx += 4.0;
        for ((filter, label), width) in filters.into_iter().zip(widths) {
            let rect = Rect::new(sx, y + 4.0, width, 24.0);
            let id = Id::LogFilter(filter);
            let selected = self.logs.filter == filter;
            let (hover, _) = self.interact(id, rect, true);
            if selected {
                self.gfx.fill(rect, 4.0, th.control);
                self.gfx.stroke(rect, 4.0, 1.0, th.control_stroke);
                self.gfx.fill(Rect::new(rect.x + rect.w / 2.0 - 8.0, rect.bottom() - 3.0, 16.0, 3.0), 1.5, th.accent);
            } else {
                self.gfx.fill(rect, 4.0, th.subtle_hover.alpha(hover));
            }
            let color = if selected { th.text } else { th.text_secondary };
            self.gfx.text(label, Font::Body, rect, color, Align::Center);
            sx += width;
        }

        let verbose = crate::logbuf::buffer().verbose();
        let right = x + w;
        self.gfx.text(t("logs.verbose"), Font::Body, Rect::new(right - 40.0 - 112.0, y + 6.0, 100.0, 20.0), th.text, Align::End);
        self.toggle(Id::LogVerbose, right - 40.0, y + 16.0, verbose, true);

        if self.logs.stale {
            self.logs.entries = crate::logbuf::buffer().snapshot(self.logs.filter.level());
            self.logs.stale = false;
        }
        let count = self.logs.entries.len();
        let count_text = tf(if count == 1 { "logs.entry" } else { "logs.entries" }, &[("count", &count)]);
        self.gfx.text(&count_text, Font::Caption, Rect::new(seg.right() + 16.0, y, 200.0, 32.0), th.text_tertiary, Align::Start);

        let card = Rect::new(x, y + 48.0, w, (h - y - 48.0 - 24.0).max(120.0));
        self.gfx.fill(card, 8.0, if th.dark { Color::argb(0x4D00_0000) } else { Color::argb(0xCCFF_FFFF) });
        self.gfx.stroke(card, 8.0, 1.0, th.card_stroke);
        let inner = card.inset(1.0, 1.0);
        self.region(inner, Id::LogView);
        self.logs.view = inner;

        let text_w = (inner.w - 16.0 - TIME_W - LEVEL_W - 24.0).max(80.0);
        if (self.logs.layout_width - text_w).abs() > 0.5 {
            self.logs.heights.clear();
            self.logs.layout_width = text_w;
        }
        let mut heights = Vec::with_capacity(count);
        let mut total_h = 8.0;
        for entry in &self.logs.entries {
            let h = match self.logs.heights.get(&entry.id) {
                Some(h) => *h,
                None => {
                    let (_, mh) = self.gfx.measure(&entry.text, Font::Mono, text_w, true);
                    let h = mh.max(16.0) + 2.0 * LINE_PAD;
                    self.logs.heights.insert(entry.id, h);
                    h
                }
            };
            heights.push(h);
            total_h += h;
        }
        if self.logs.heights.len() > count * 2 + 512 {
            let live: std::collections::HashSet<u64> = self.logs.entries.iter().map(|e| e.id).collect();
            self.logs.heights.retain(|id, _| live.contains(id));
        }
        total_h += 8.0;
        self.logs.max_scroll = (total_h - inner.h).max(0.0);
        if self.logs.follow {
            self.logs.scroll = self.logs.max_scroll;
        }
        self.logs.scroll = self.logs.scroll.clamp(0.0, self.logs.max_scroll);

        if count == 0 {
            self.gfx.glyph(glyph::LOG, Font::IconLarge, inner.x + inner.w / 2.0, inner.y + inner.h / 2.0 - 18.0, th.text_tertiary);
            self.gfx.text(t("logs.empty"), Font::Body, Rect::new(inner.x, inner.y + inner.h / 2.0 + 4.0, inner.w, 24.0), th.text_secondary, Align::Center);
            return;
        }

        self.gfx.push_clip(inner);
        let mut ry = inner.y + 8.0 - self.logs.scroll;
        for (i, entry) in self.logs.entries.iter().enumerate() {
            let row_h = heights[i];
            if ry + row_h < inner.y {
                ry += row_h;
                continue;
            }
            if ry > inner.bottom() {
                break;
            }
            let color = self.level_color(entry.level);
            if entry.level <= Level::WARN {
                self.gfx.fill(Rect::new(inner.x + 4.0, ry, inner.w - 8.0, row_h), 4.0, color.alpha(0.08));
            }
            let lx = inner.x + 16.0;
            self.gfx.text_top(&entry.time, Font::Mono, Rect::new(lx, ry + LINE_PAD, TIME_W, 18.0), th.text_tertiary, Align::Start);
            let label = level_label(entry.level);
            self.gfx.text_top(label, Font::Mono, Rect::new(lx + TIME_W, ry + LINE_PAD, LEVEL_W, 18.0), color, Align::Start);
            let text_color = if entry.level >= Level::DEBUG { th.text_secondary } else { th.text };
            self.gfx.paragraph(&entry.text, Font::Mono, Rect::new(lx + TIME_W + LEVEL_W, ry + LINE_PAD, text_w + 4.0, row_h), text_color);
            ry += row_h;
        }
        self.gfx.pop_clip();

        if self.logs.max_scroll > 0.0 {
            let track = inner.h - 12.0;
            let thumb = (inner.h / total_h * track).max(28.0);
            let pos = inner.y + 6.0 + (track - thumb) * (self.logs.scroll / self.logs.max_scroll);
            self.gfx.fill(Rect::new(inner.right() - 6.0, pos, 3.0, thumb), 1.5, th.control_strong.alpha(0.8));
        }
        if !self.logs.follow {
            let label = t("logs.follow");
            let bw = self.button_width(label, None);
            let rect = Rect::new(inner.right() - bw - 16.0, inner.bottom() - 48.0, bw, 32.0);
            self.shadow(rect, 4.0, 8.0);
            self.button(Id::LogFollow, rect, label, None, Style::Accent, true);
        }
    }
}

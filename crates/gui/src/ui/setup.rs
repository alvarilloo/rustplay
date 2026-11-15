// First-run setup assistant (system check), drawn as a full-size dialog over the window. It is shown
// automatically the first time RustPlay runs and from Settings → «Comprobar el sistema».

use std::sync::mpsc;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::widgets::Style;
use super::{App, GAP, Id, RECEIVER_PORT, ROW_H, ReceiverState, TITLE_H, glyph};
use crate::gfx::{Align, Color, Font, Rect};
use crate::i18n::{t, tf};
use crate::syscheck::{self, CheckResult, Fix, Status};

mod icon {
    pub const OK: char = '\u{E73E}';
    pub const WARNING: char = '\u{E7BA}';
    pub const PROBLEM: char = '\u{E711}';
    pub const CHECKUP: char = '\u{E9D9}';
}

struct Done {
    results: Vec<CheckResult>,
    // Why the elevated fix did not complete (UAC declined, netsh failed).
    fix_error: Option<String>,
}

pub struct Setup {
    results: Vec<CheckResult>,
    rx: Option<mpsc::Receiver<Done>>,
    // The design preview shows the «checking» state without a worker.
    pretend_busy: bool,
    fixing: Option<Fix>,
    fix_error: Option<String>,
    scroll: f32,
    scroll_max: f32,
    view: Rect,
}

impl Setup {
    fn new() -> Self {
        Self { results: Vec::new(), rx: None, pretend_busy: false, fixing: None, fix_error: None, scroll: 0.0, scroll_max: 0.0, view: Rect::default() }
    }

    fn busy(&self) -> bool {
        self.rx.is_some() || self.pretend_busy
    }
}

const DIALOG_W: f32 = 780.0;
const FOOTER_H: f32 = 80.0;

impl App {
    // Opens the assistant and starts the checks.
    pub(super) fn open_setup(&mut self) {
        self.combo = None;
        self.anims.remove(&(Id::Modal, 2));
        self.setup = Some(Setup::new());
        self.kb_focus = None;
        if !self.settings.onboarding_done {
            self.settings.onboarding_done = true;
            self.settings.save();
        }
        self.start_checks(None);
    }

    pub fn preview_setup(&mut self, busy: bool) {
        let mut setup = Setup::new();
        if busy {
            setup.pretend_busy = true;
        } else {
            setup.results = syscheck::sample_results();
        }
        self.anims.remove(&(Id::Modal, 2));
        self.setup = Some(setup);
    }

    pub fn preview_setup_close(&mut self) {
        self.setup = None;
    }

    fn start_checks(&mut self, fix: Option<Fix>) {
        let Some(setup) = self.setup.as_mut() else { return };
        if setup.rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let receiver_running = matches!(self.receiver, ReceiverState::On { .. } | ReceiverState::Starting);
        let hwnd = self.hwnd.0 as isize;
        let spawned = std::thread::Builder::new().name(String::from("rustplay-syscheck")).spawn(move || {
            let mut fix_error = None;
            if fix == Some(Fix::AddFirewallRule) {
                tracing::info!("el usuario pidió añadir la regla del Firewall de Windows");
                if let Err(e) = syscheck::add_firewall_rule() {
                    tracing::warn!("no se pudo añadir la regla del Firewall: {e}");
                    fix_error = Some(e);
                }
            }
            let results = syscheck::run_all(RECEIVER_PORT, receiver_running);
            for r in &results {
                tracing::info!("comprobación del sistema: {:?} {:?} {} {}", r.id, r.status, r.message, r.detail);
            }
            if tx.send(Done { results, fix_error }).is_ok() {
                // SAFETY: posting to a window handle is allowed from any thread.
                unsafe {
                    let _ = PostMessageW(Some(windows::Win32::Foundation::HWND(hwnd as _)), crate::backend::WM_BACKEND, WPARAM(0), LPARAM(0));
                }
            }
        });
        if spawned.is_ok() {
            setup.rx = Some(rx);
            setup.fixing = fix;
            setup.fix_error = None;
            setup.results.clear();
            setup.scroll = 0.0;
        }
    }

    pub(super) fn poll_setup(&mut self) {
        let Some(setup) = self.setup.as_mut() else { return };
        let Some(rx) = &setup.rx else { return };
        match rx.try_recv() {
            Ok(done) => {
                setup.results = done.results;
                setup.fix_error = done.fix_error;
                setup.rx = None;
                setup.fixing = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                setup.rx = None;
                setup.fixing = None;
            }
        }
    }

    pub(super) fn setup_wheel(&mut self, delta: f32) -> bool {
        let Some(setup) = self.setup.as_mut() else { return false };
        setup.scroll = (setup.scroll - delta * 80.0).clamp(0.0, setup.scroll_max);
        true
    }

    // Keeps a control reached with the keyboard inside the visible part of the list.
    pub(super) fn setup_reveal(&mut self, rect: Rect) {
        let Some(setup) = self.setup.as_mut() else { return };
        let view = setup.view;
        if rect.y < view.y {
            setup.scroll = (setup.scroll - (view.y - rect.y) - 8.0).max(0.0);
        } else if rect.bottom() > view.bottom() {
            setup.scroll = (setup.scroll + rect.bottom() - view.bottom() + 8.0).min(setup.scroll_max);
        }
    }

    pub(super) fn setup_open(&self) -> bool {
        self.setup.is_some()
    }

    pub(super) fn close_setup(&mut self) {
        self.setup = None;
        self.kb_focus = None;
    }

    pub(super) fn setup_click(&mut self, id: Id) {
        match id {
            Id::SetupOpen => self.open_setup(),
            Id::SetupClose => self.close_setup(),
            Id::SetupRecheck => self.start_checks(None),
            Id::SetupCopy => self.copy_setup_report(),
            Id::SetupFix(index) => {
                let Some(setup) = self.setup.as_ref() else { return };
                if setup.busy() {
                    return;
                }
                match setup.results.get(index).and_then(|r| r.fix) {
                    Some(Fix::OpenNetworkSettings) => syscheck::open_network_settings(),
                    Some(Fix::OpenFirewall) => syscheck::open_firewall_console(),
                    Some(Fix::AddFirewallRule) => self.start_checks(Some(Fix::AddFirewallRule)),
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn result_text(result: &CheckResult) -> (String, String) {
        (t(result.id.title_key()).to_owned(), tf(result.message, &[("detail", &result.detail)]))
    }

    fn copy_setup_report(&mut self) {
        let Some(setup) = self.setup.as_ref() else { return };
        if setup.results.is_empty() {
            return;
        }
        let text = syscheck::report(env!("CARGO_PKG_VERSION"), &setup.results, Self::result_text);
        if super::logs::set_clipboard(self.hwnd, &text) {
            self.toast(t("setup.copied"));
        }
    }

    fn status_color(&self, status: Status) -> Color {
        match status {
            Status::Ok => self.theme.success,
            Status::Warning => self.theme.caution,
            Status::Problem => self.theme.critical,
        }
    }

    // Row of the settings page that opens the assistant; returns the new y.
    pub(super) fn draw_setup_settings(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        self.section(x, y, w, t("settings.system"));
        y += 30.0;
        let label = t("setup.check_now");
        let bw = self.button_width(label, None);
        let rect = self.row(Rect::new(x, y, w, ROW_H), icon::CHECKUP, t("settings.system_check"), t("settings.system_check_desc"), bw + 16.0, true);
        self.button(Id::SetupOpen, Rect::new(rect.right() - 16.0 - bw, rect.y + 18.0, bw, 32.0), label, None, Style::Standard, true);
        let y = y + ROW_H + GAP;
        self.draw_welcome_settings(x, y, w) + 28.0
    }

    pub(super) fn draw_setup(&mut self, w: f32, h: f32) {
        let th = self.theme;
        let Some(setup) = self.setup.as_ref() else { return };
        let busy = setup.busy();
        let results = setup.results.clone();
        let fixing = setup.fixing;
        let fix_error = setup.fix_error.clone();
        let scroll = setup.scroll;
        let fade = self.anim((Id::Modal, 2), 1.0, 16.0);
        self.gfx.fill(Rect::new(0.0, 0.0, w, h), 0.0, th.smoke.alpha(fade));
        self.region(Rect::new(0.0, TITLE_H, w, h - TITLE_H), Id::Modal);

        let dw = (w - 64.0).min(DIALOG_W);
        let dh = (h - TITLE_H - 40.0).min(780.0);
        let rect = Rect::new((w - dw) / 2.0, TITLE_H + (h - TITLE_H - dh) / 2.0 + 16.0 * (1.0 - fade), dw, dh);
        self.gfx.push_layer(fade, 0.0);
        self.shadow(rect, 8.0, 32.0);
        self.gfx.fill(rect, 8.0, th.dialog);
        let footer = Rect::new(rect.x, rect.bottom() - FOOTER_H, dw, FOOTER_H);
        self.gfx.fill(footer, 8.0, th.dialog_footer);
        self.gfx.fill(Rect::new(footer.x, footer.y, dw, 12.0), 0.0, th.dialog_footer);
        self.gfx.line(footer.x, footer.y, footer.right(), footer.y, 1.0, th.card_stroke);
        self.gfx.stroke(rect, 8.0, 1.0, th.flyout_stroke);

        // Header: title, summary pill (or spinner) and one line of explanation.
        let x = rect.x + 28.0;
        let inner_w = dw - 56.0;
        self.gfx.text(t("setup.title"), Font::Subtitle, Rect::new(x, rect.y + 24.0, inner_w - 200.0, 28.0), th.text, Align::Start);
        let pill_cy = rect.y + 38.0;
        if busy {
            self.spinner(rect.right() - 28.0 - 10.0, pill_cy, 9.0, th.accent);
        } else if !results.is_empty() {
            let overall = syscheck::overall(&results);
            let text = match overall {
                Status::Ok => t("setup.summary.ok").to_owned(),
                Status::Warning => tf("setup.summary.warnings", &[("count", &syscheck::count(&results, Status::Warning))]),
                Status::Problem => tf("setup.summary.problems", &[("count", &syscheck::count(&results, Status::Problem))]),
            };
            let bw = self.badge_width(&text);
            self.badge(rect.right() - 28.0 - bw, pill_cy, &text, self.status_color(overall));
        }
        let intro = if busy { t("setup.checking") } else { t("setup.subtitle") };
        let (_, intro_h) = self.gfx.measure(intro, Font::Body, inner_w, true);
        self.gfx.paragraph(intro, Font::Body, Rect::new(x, rect.y + 60.0, inner_w, intro_h + 4.0), th.text_secondary);

        // Scrolling list of checks.
        let list_top = rect.y + 60.0 + intro_h + 16.0;
        let view = Rect::new(rect.x, list_top, dw, footer.y - list_top);
        let row_x = x;
        let row_w = inner_w - 10.0;
        let mut layout: Vec<(Rect, f32)> = Vec::new();
        let mut cursor = 0.0f32;
        let entries: Vec<(crate::syscheck::CheckId, Option<&CheckResult>)> =
            syscheck::CheckId::ALL.iter().map(|id| (*id, results.iter().find(|r| r.id == *id))).collect();
        for (_, result) in &entries {
            let text_w = row_w - 60.0 - 16.0 - result.and_then(|r| r.fix).map_or(0.0, |f| self.button_width(t(f.label_key()), None) + 16.0);
            let body = result.map_or_else(|| t("setup.running").to_owned(), |r| Self::result_text(r).1);
            let (_, body_h) = self.gfx.measure(&body, Font::Body, text_w.max(80.0), true);
            let row_h = (20.0 + 22.0 + body_h + 16.0).max(ROW_H - 4.0);
            layout.push((Rect::new(row_x, cursor, row_w, row_h), body_h));
            cursor += row_h + GAP;
        }
        let error_h = if fix_error.is_some() { 44.0 } else { 0.0 };
        let content = cursor + error_h;
        let scroll_max = (content - view.h + 8.0).max(0.0);
        let scroll = scroll.min(scroll_max);
        if let Some(setup) = self.setup.as_mut() {
            setup.scroll = scroll;
            setup.scroll_max = scroll_max;
            setup.view = view;
        }
        self.clipped(view, |app| {
            for (i, ((id, result), (local, body_h))) in entries.iter().zip(&layout).enumerate() {
                let row = Rect::new(local.x, view.y + local.y - scroll, local.w, local.h);
                if row.bottom() < view.y - 4.0 || row.y > view.bottom() + 4.0 {
                    // Off screen, but its controls still take part in the Tab order.
                    if let Some(r) = result.filter(|r| r.fix.is_some()) {
                        let bw = app.button_width(t(r.fix.unwrap_or(Fix::OpenFirewall).label_key()), None);
                        app.note_focusable(Rect::new(row.right() - 16.0 - bw, row.y + (row.h - 32.0) / 2.0, bw, 32.0), Id::SetupFix(i));
                    }
                    continue;
                }
                app.card(row);
                let cy = row.y + row.h / 2.0;
                let title = t(id.title_key());
                match result {
                    Some(r) => {
                        let color = app.status_color(r.status);
                        app.gfx.circle(row.x + 30.0, cy, 16.0, color.alpha(0.18));
                        let glyph = match r.status {
                            Status::Ok => icon::OK,
                            Status::Warning => icon::WARNING,
                            Status::Problem => icon::PROBLEM,
                        };
                        app.gfx.glyph(glyph, Font::Icon, row.x + 30.0, cy, color);
                    }
                    None => app.spinner(row.x + 30.0, cy, 9.0, th.accent),
                }
                let fix = result.and_then(|r| r.fix);
                let fix_w = fix.map_or(0.0, |f| app.button_width(t(f.label_key()), None));
                let tx = row.x + 60.0;
                let tw = row.w - 60.0 - 16.0 - if fix.is_some() { fix_w + 16.0 } else { 0.0 };
                app.gfx.text(title, Font::BodyStrong, Rect::new(tx, row.y + 12.0, tw, 20.0), th.text, Align::Start);
                let body = result.map_or_else(|| t("setup.running").to_owned(), |r| Self::result_text(r).1);
                app.gfx.paragraph(&body, Font::Body, Rect::new(tx, row.y + 34.0, tw, body_h + 4.0), th.text_secondary);
                if let Some(fix) = fix {
                    let button = Rect::new(row.right() - 16.0 - fix_w, cy - 16.0, fix_w, 32.0);
                    if fixing == Some(fix) {
                        app.spinner(button.x - 20.0, cy, 8.0, th.accent);
                    }
                    app.button(Id::SetupFix(i), button, t(fix.label_key()), None, Style::Standard, !busy);
                }
            }
            if let Some(error) = &fix_error {
                let y = view.y + cursor - scroll;
                let text = tf("setup.fix_failed", &[("error", error)]);
                app.gfx.paragraph(&text, Font::Caption, Rect::new(row_x + 4.0, y, row_w - 8.0, 40.0), th.critical);
            }
        });
        if scroll_max > 0.0 {
            let track = view.h - 16.0;
            let thumb = (view.h / (content + 8.0) * track).max(32.0);
            let pos = view.y + 8.0 + (track - thumb) * (scroll / scroll_max);
            self.gfx.fill(Rect::new(rect.right() - 12.0, pos, 3.0, thumb), 1.5, th.control_strong.alpha(0.8));
        }

        // Footer: report actions on the left, the primary action on the right.
        let fy = footer.y + 24.0;
        let mut bx = x;
        for (id, label, glyph, enabled) in [
            (Id::SetupCopy, t("setup.copy"), glyph::COPY, !busy && !results.is_empty()),
            (Id::SetupRecheck, t("setup.recheck"), glyph::REFRESH, !busy),
        ] {
            let bw = self.button_width(label, Some(glyph));
            self.button(id, Rect::new(bx, fy, bw, 32.0), label, Some(glyph), Style::Standard, enabled);
            bx += bw + 8.0;
        }
        let done = t("setup.done");
        let dw_btn = self.button_width(done, None).max(120.0);
        self.button(Id::SetupClose, Rect::new(rect.right() - 28.0 - dw_btn, fy, dw_btn, 32.0), done, None, Style::Accent, true);
        self.gfx.pop_layer();
    }
}

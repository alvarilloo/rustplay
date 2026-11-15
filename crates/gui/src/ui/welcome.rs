// Welcome tour: a full-window, animated introduction to RustPlay with a mini tutorial. It covers the
// whole window (aurora background, floating particles, scenes that move), is shown the first time
// the window opens and from Settings → «Introducción a RustPlay». The last page offers the system
// check (see setup.rs).

use std::time::Instant;

use super::widgets::{Link, Style};
use super::{App, GAP, Id, ROW_H, TITLE_H, glyph};
use crate::gfx::{Align, Color, Font, Rect};
use crate::i18n::{t, tf};

const STEPS: usize = 6;
const STAGE_W: f32 = 980.0;
const BAR_H: f32 = 108.0;
const SLIDE_SECONDS: f32 = 0.28;

mod icon {
    pub const TOUCH: char = '\u{E7C9}';
    pub const KEYBOARD: char = '\u{E765}';
    pub const SETTINGS: char = '\u{E713}';
    pub const RECORD: char = '\u{E7C8}';
    pub const WAVE: char = '\u{E9D9}';
    pub const FULLSCREEN: char = '\u{E740}';
    pub const CAMERA: char = '\u{E722}';
    pub const STOP: char = '\u{E711}';
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - clamp01(t)).powi(3)
}

fn ease_out_back(t: f32) -> f32 {
    let t = clamp01(t);
    let (c1, c3) = (1.70158f32, 2.70158f32);
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

// Progress (0..1) of an entrance that starts `delay` seconds into the page and lasts `length`.
fn enter(page_time: f32, delay: f32, length: f32) -> f32 {
    ease_out((page_time - delay) / length)
}

// Deterministic pseudo-random number in 0..1 for particle `i` and stream `k`.
fn rand(i: u32, k: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9) ^ k.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x & 0xFFFF) as f32 / 65535.0
}

const PINK: (u8, u8, u8) = (0xFF, 0x4F, 0x8B);
const AMBER: (u8, u8, u8) = (0xFF, 0xB3, 0x47);

fn brand(c: (u8, u8, u8)) -> Color {
    Color::rgb(c.0, c.1, c.2)
}

impl App {
    pub(super) fn welcome_open(&self) -> bool {
        self.welcome.is_some()
    }

    pub(super) fn open_welcome(&mut self) {
        self.combo = None;
        self.kb_focus = None;
        self.anims.remove(&(Id::Modal, 3));
        self.welcome = Some(0);
        self.welcome_at = Instant::now();
        self.welcome_prev = None;
        if !self.settings.onboarding_done {
            self.settings.onboarding_done = true;
            self.settings.save();
        }
    }

    pub fn preview_welcome(&mut self, step: Option<usize>) {
        self.anims.remove(&(Id::Modal, 3));
        self.welcome = step.map(|s| s.min(STEPS - 1));
        // The captures show each page a little after it has come in.
        self.welcome_at = Instant::now().checked_sub(std::time::Duration::from_millis(2400)).unwrap_or_else(Instant::now);
        self.welcome_prev = None;
    }

    // Shows the tour by itself the first time, once the start-up animation has ended.
    pub(super) fn maybe_open_onboarding(&mut self) {
        if !self.settings.onboarding_done
            && self.welcome.is_none()
            && self.setup.is_none()
            && self.splash_time().is_none()
            && self.visible
            && !crate::preview::active()
        {
            self.open_welcome();
        }
    }

    pub(super) fn close_welcome(&mut self) {
        self.welcome = None;
        self.welcome_prev = None;
        self.kb_focus = None;
    }

    fn welcome_go(&mut self, step: usize) {
        let Some(current) = self.welcome else { return };
        let step = step.min(STEPS - 1);
        if step != current {
            self.welcome_prev = Some((current, Instant::now(), if step > current { -1.0 } else { 1.0 }));
            self.welcome = Some(step);
            self.welcome_at = Instant::now();
        }
    }

    pub(super) fn welcome_step(&self) -> usize {
        self.welcome.unwrap_or(0)
    }

    pub(super) fn welcome_next(&mut self) {
        let step = self.welcome_step();
        if step + 1 >= STEPS {
            self.close_welcome();
        } else {
            self.welcome_go(step + 1);
        }
    }

    pub(super) fn welcome_back(&mut self) {
        let step = self.welcome_step();
        self.welcome_go(step.saturating_sub(1));
    }

    pub(super) fn welcome_click(&mut self, id: Id) {
        match id {
            Id::WelcomeOpen => self.open_welcome(),
            Id::WelcomeNext => self.welcome_next(),
            Id::WelcomeBack => self.welcome_back(),
            Id::WelcomeSkip => self.close_welcome(),
            Id::WelcomeDot(i) => self.welcome_go(i),
            Id::WelcomeCheck => {
                self.close_welcome();
                self.open_setup();
            }
            _ => {}
        }
    }

    // Settings row that reopens the tour; returns the new y.
    pub(super) fn draw_welcome_settings(&mut self, x: f32, y: f32, w: f32) -> f32 {
        let label = t("welcome.open");
        let bw = self.button_width(label, None);
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::PEOPLE, t("settings.welcome"), t("settings.welcome_desc"), bw + 16.0, true);
        self.button(Id::WelcomeOpen, Rect::new(rect.right() - 16.0 - bw, rect.y + 18.0, bw, 32.0), label, None, Style::Standard, true);
        y + ROW_H + GAP
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn draw_welcome(&mut self, w: f32, h: f32) {
        let th = self.theme;
        self.animating = true;
        let step = self.welcome_step();
        let time = self.started.elapsed().as_secs_f32();
        let fade = self.anim((Id::Modal, 3), 1.0, 7.0);
        let stage_top = TITLE_H;
        self.region(Rect::new(0.0, stage_top, w, h - stage_top), Id::Modal);

        // Opaque, so nothing of the app shows through; the first frames fade in from the window.
        self.gfx.push_layer(fade, 0.0);
        let area = Rect::new(0.0, stage_top, w, h - stage_top);
        self.gfx.fill(area, 0.0, th.solid_background);
        self.draw_aurora(area, time, if th.dark { 1.0 } else { 0.6 });
        self.draw_particles(area, time);

        let stage_w = STAGE_W.min(w - 48.0);
        let stage = Rect::new((w - stage_w) / 2.0, stage_top + 24.0, stage_w, h - stage_top - BAR_H - 36.0);
        let page_time = self.welcome_at.elapsed().as_secs_f32();
        if let Some((prev, since, dir)) = self.welcome_prev {
            let k = since.elapsed().as_secs_f32() / SLIDE_SECONDS;
            if k >= 1.0 {
                self.welcome_prev = None;
            } else {
                // The page that leaves slides out and fades.
                let e = ease_out(k);
                self.gfx.push_layer(1.0 - e, dir * 60.0 * e);
                self.draw_welcome_page(prev, stage, 9.0, time);
                self.gfx.pop_layer();
            }
        }
        let slide = self.welcome_prev.map_or(1.0, |(_, since, _)| ease_out(since.elapsed().as_secs_f32() / SLIDE_SECONDS));
        let dir = self.welcome_prev.map_or(0.0, |(_, _, d)| d);
        self.gfx.push_layer(slide, -dir * 60.0 * (1.0 - slide));
        self.draw_welcome_page(step, stage, page_time, time);
        self.gfx.pop_layer();

        self.draw_welcome_bar(w, h, step);
        self.gfx.pop_layer();
    }

    // Soft moving lights over the background.
    fn draw_aurora(&self, area: Rect, time: f32, strength: f32) {
        let th = self.theme;
        if th.high_contrast {
            return;
        }
        let lights = [(th.accent, 0.18, 0.30, 0.28, 0.9), (brand(PINK), 0.78, 0.22, 0.24, 0.7), (brand(AMBER), 0.55, 0.92, 0.30, 0.6)];
        for (i, (color, bx, by, size, weight)) in lights.into_iter().enumerate() {
            let phase = i as f32 * 2.1;
            let cx = area.x + area.w * (bx + 0.10 * (time * 0.13 + phase).sin());
            let cy = area.y + area.h * (by + 0.12 * (time * 0.11 + phase * 1.3).cos());
            self.gfx.glow_ellipse(cx, cy, area.w * size * 1.6, area.h * size * 1.4, color.alpha(0.30 * weight * strength));
        }
    }

    // Small lights drifting upwards, like dust in a projector beam.
    fn draw_particles(&self, area: Rect, time: f32) {
        let th = self.theme;
        if th.high_contrast {
            return;
        }
        for i in 0..36u32 {
            let speed = 6.0 + 16.0 * rand(i, 1);
            let x = area.x + area.w * (rand(i, 2) + 0.02 * (time * 0.4 + i as f32).sin());
            let y = area.bottom() - (time * speed + rand(i, 3) * area.h * 1.5).rem_euclid(area.h + 40.0) + 20.0;
            let twinkle = 0.5 + 0.5 * (time * (0.8 + rand(i, 4)) + i as f32).sin();
            let r = 1.2 + 2.2 * rand(i, 5);
            self.gfx.circle(x, y, r, th.text.alpha(0.10 + 0.20 * twinkle));
        }
    }

    // Bottom bar: page dots, back and next, and the skip button at the top right.
    fn draw_welcome_bar(&mut self, w: f32, h: f32, step: usize) {
        let th = self.theme;
        let last = step + 1 == STEPS;
        let cy = h - BAR_H / 2.0 - 4.0;
        let dots_w = STEPS as f32 * 24.0;
        let mut dx = w / 2.0 - dots_w / 2.0;
        for i in 0..STEPS {
            let rect = Rect::new(dx, cy - 14.0, 24.0, 28.0);
            let (hover, _) = self.interact(Id::WelcomeDot(i), rect, true);
            let current = i == step;
            let width = if current { 22.0 } else { 7.0 + 2.0 * hover };
            let color = if current { th.accent } else { th.text_tertiary.alpha(0.5 + 0.4 * hover) };
            let pill = Rect::new(dx + 12.0 - width / 2.0, cy - 3.5, width, 7.0);
            self.gfx.fill(pill, 3.5, color);
            dx += 24.0;
        }
        let primary = if step == 0 { t("welcome.start") } else if last { t("welcome.finish") } else { t("welcome.next") };
        let pw = self.button_width(primary, None).max(150.0) + 24.0;
        let bx = w / 2.0 + dots_w / 2.0 + 32.0;
        self.button(Id::WelcomeNext, Rect::new(bx, cy - 20.0, pw, 40.0), primary, None, Style::Accent, true);
        if step > 0 {
            let back = t("welcome.back");
            let bw = self.button_width(back, None).max(110.0);
            self.button(Id::WelcomeBack, Rect::new(w / 2.0 - dots_w / 2.0 - 32.0 - bw, cy - 20.0, bw, 40.0), back, None, Style::Standard, true);
        }
        // Top right: skip, or on the last page the system check.
        let (label, id) = if last { (t("welcome.check"), Id::WelcomeCheck) } else { (t("welcome.skip"), Id::WelcomeSkip) };
        let sw = self.button_width(label, None);
        self.button(id, Rect::new(w - 24.0 - sw, TITLE_H + 14.0, sw, 32.0), label, None, Style::Subtle, true);
    }

    fn draw_welcome_page(&mut self, step: usize, stage: Rect, pt: f32, time: f32) {
        match step {
            0 => self.page_welcome(stage, pt, time),
            1 => self.page_features(stage, pt, time),
            2 => self.page_link(stage, pt, time, true),
            3 => self.page_link(stage, pt, time, false),
            4 => self.page_tips(stage, pt, time),
            _ => self.page_done(stage, pt),
        }
    }

    // Draws `body` in a layer that fades in with a small rise.
    fn rise(&mut self, pt: f32, delay: f32, body: impl FnOnce(&mut Self)) {
        let e = enter(pt, delay, 0.55);
        self.gfx.push_layer(e, 22.0 * (1.0 - e));
        body(self);
        self.gfx.pop_layer();
    }

    fn heading(&mut self, x: f32, y: f32, w: f32, title: &'static str, subtitle: &'static str, pt: f32) -> f32 {
        let th = self.theme;
        self.rise(pt, 0.05, |app| app.gfx.text(title, Font::Display, Rect::new(x, y, w, 52.0), th.text, Align::Start));
        let sw = w.min(640.0);
        let (_, h) = self.gfx.measure(subtitle, Font::BodyLarge, sw, true);
        self.rise(pt, 0.18, |app| {
            app.gfx.paragraph(subtitle, Font::BodyLarge, Rect::new(x, y + 58.0, sw, h + 4.0), th.text_secondary);
        });
        y + 58.0 + h + 28.0
    }

    // ------------------------------------------------------------------ pages

    fn page_welcome(&mut self, stage: Rect, pt: f32, time: f32) {
        let th = self.theme;
        let cx = stage.x + stage.w / 2.0;
        let cy = stage.y + stage.h * 0.36;
        // Rings that spread from the logo, and the devices RustPlay connects orbiting it.
        if !th.high_contrast {
            for k in 0..3 {
                let phase = (time * 0.32 + k as f32 / 3.0).rem_euclid(1.0);
                let r = 70.0 + 150.0 * phase;
                self.gfx.stroke(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r), r, 2.0, th.accent.alpha(0.45 * (1.0 - phase).powi(2)));
            }
            self.gfx.glow(cx, cy, 190.0, th.accent.alpha(0.25 * enter(pt, 0.0, 0.8)));
        }
        let orbit = [glyph::PHONE, glyph::TV, glyph::PC];
        for (i, g) in orbit.into_iter().enumerate() {
            let a = time * 0.35 + i as f32 * std::f32::consts::TAU / 3.0;
            let appear = enter(pt, 0.5 + i as f32 * 0.15, 0.6);
            let (ox, oy) = (cx + 176.0 * a.cos(), cy + 84.0 * a.sin());
            let depth = 0.75 + 0.25 * a.sin();
            let r = 26.0 * depth * appear;
            self.gfx.circle(ox, oy, r, th.card.mix(th.accent, 0.16));
            self.gfx.stroke(Rect::new(ox - r, oy - r, 2.0 * r, 2.0 * r), r, 1.0, th.accent.alpha(0.5 * appear));
            self.gfx.glyph(g, Font::Icon20, ox, oy, th.text.alpha(appear));
        }
        if let Some(logo) = self.logo.clone() {
            let pop = ease_out_back(pt / 0.7);
            let size = 128.0 * pop.max(0.0) * (1.0 + 0.015 * (time * 1.6).sin());
            self.gfx.image(&logo, Rect::new(cx - size / 2.0, cy - size / 2.0, size, size), clamp01(pt / 0.25));
        }
        let ty = cy + 120.0;
        self.rise(pt, 0.55, |app| app.gfx.text(t("welcome.title"), Font::Display, Rect::new(stage.x, ty, stage.w, 52.0), th.text, Align::Center));
        let body = t("welcome.subtitle");
        let bw = (stage.w - 160.0).min(700.0);
        let (_, bh) = self.gfx.measure(body, Font::BodyLarge, bw, true);
        self.rise(pt, 0.8, |app| {
            app.gfx.paragraph(body, Font::BodyLarge, Rect::new(stage.x + (stage.w - bw) / 2.0, ty + 62.0, bw, bh + 4.0), th.text_secondary);
        });
        self.rise(pt, 1.05, |app| {
            app.gfx.text(t("welcome.tagline"), Font::Subtitle, Rect::new(stage.x, ty + 78.0 + bh, stage.w, 32.0), th.accent, Align::Center);
        });
    }

    fn page_features(&mut self, stage: Rect, pt: f32, time: f32) {
        let th = self.theme;
        let y = self.heading(stage.x, stage.y, stage.w, t("welcome.features.title"), t("welcome.features.subtitle"), pt);
        let cw = (stage.w - GAP) / 2.0;
        let ch = ((stage.bottom() - y - GAP) / 2.0).clamp(110.0, 150.0);
        let cards = [
            (glyph::RECEIVE, "welcome.feature.receive.title", "welcome.feature.receive.body"),
            (glyph::CAST, "welcome.feature.send.title", "welcome.feature.send.body"),
            (glyph::VIDEO, "welcome.feature.apps.title", "welcome.feature.apps.body"),
            (icon::RECORD, "welcome.feature.record.title", "welcome.feature.record.body"),
        ];
        for (i, (g, title, body)) in cards.into_iter().enumerate() {
            let rect = Rect::new(stage.x + (i % 2) as f32 * (cw + GAP), y + (i / 2) as f32 * (ch + GAP), cw, ch);
            let e = enter(pt, 0.30 + i as f32 * 0.12, 0.6);
            self.gfx.push_layer(e, 30.0 * (1.0 - e));
            self.card(rect);
            let tile = Rect::new(rect.x + 22.0, rect.y + 22.0, 52.0, 52.0);
            // Each icon has its own small loop: waves, a flying comet, a progress bar, a red dot.
            self.gfx.fill(tile, 12.0, th.accent.alpha(0.16));
            let (tx, ty) = (tile.x + 26.0, tile.y + 26.0);
            let beat = 0.5 + 0.5 * (time * 2.2 + i as f32).sin();
            match i {
                0 => {
                    let p = (time * 0.9).rem_euclid(1.0);
                    let r = 28.0 + 12.0 * p;
                    self.gfx.stroke(Rect::new(tx - r, ty - r, 2.0 * r, 2.0 * r), r, 1.5, th.accent.alpha(0.5 * (1.0 - p)));
                }
                1 => {
                    let p = (time * 0.7).rem_euclid(1.0);
                    self.gfx.circle(tile.x + 8.0 + p * 36.0, tile.bottom() + 8.0, 3.0, th.accent.alpha(1.0 - p));
                }
                2 => self.gfx.fill(Rect::new(tile.x + 6.0, tile.bottom() - 7.0, 40.0 * (time * 0.35).rem_euclid(1.0), 3.0), 1.5, th.accent),
                _ => self.gfx.circle(tx + 17.0, tile.y + 9.0, 3.0 + 1.5 * beat, brand((0xE8, 0x21, 0x2B))),
            }
            self.gfx.glyph(g, Font::Icon20, tx, ty, th.accent);
            let (text_x, text_w) = (tile.right() + 20.0, rect.right() - tile.right() - 40.0);
            self.gfx.text(t(title), Font::BodyStrong, Rect::new(text_x, rect.y + 20.0, text_w, 24.0), th.text, Align::Start);
            let (_, bh) = self.gfx.measure(t(body), Font::Body, text_w, true);
            self.gfx.paragraph(t(body), Font::Body, Rect::new(text_x, rect.y + 50.0, text_w, bh + 4.0), th.text_secondary);
            self.gfx.pop_layer();
        }
    }

    // The two tutorials: a scene with the devices on the left, three steps on the right.
    fn page_link(&mut self, stage: Rect, pt: f32, time: f32, receive: bool) {
        let th = self.theme;
        let name = {
            let n = self.settings.receiver_name.trim().to_owned();
            if n.is_empty() { String::from("PC") } else { n }
        };
        let (title, subtitle) = if receive {
            (t("welcome.receive.title"), t("welcome.receive.subtitle"))
        } else {
            (t("welcome.send.title"), t("welcome.send.subtitle"))
        };
        let y = self.heading(stage.x, stage.y, stage.w, title, subtitle, pt);
        let left_w = stage.w * 0.50;
        // Scene: devices linked by a light arc with comets, bigger than in the app.
        let e = enter(pt, 0.35, 0.8);
        let (sx, sy, k) = (stage.x + left_w / 2.0, y + (stage.bottom() - y) * 0.46, 1.45);
        self.gfx.push_layer(e, 0.0);
        let saved = self.gfx.begin_scale(sx, sy, k * (0.9 + 0.1 * e));
        let scene = Rect::new(sx - 190.0, sy - 105.0, 380.0, 210.0);
        if receive {
            self.link_art(scene, glyph::PC, glyph::PHONE, false, Link::Flowing, 2);
        } else {
            self.link_art(scene, glyph::PC, glyph::TV, true, Link::Flowing, 3);
        }
        self.gfx.end_scale(saved);
        self.gfx.pop_layer();

        // Steps: the active one lights up in turn, as if following along.
        let active = ((time / 2.4) as usize) % 3;
        let rows = if receive {
            [
                (tf("welcome.receive.s1_title", &[("name", &name)]), tf("welcome.receive.s1_body", &[("name", &name)])),
                (tf("welcome.receive.s2_title", &[("name", &name)]), tf("welcome.receive.s2_body", &[("name", &name)])),
                (tf("welcome.receive.s3_title", &[("name", &name)]), tf("welcome.receive.s3_body", &[("name", &name)])),
            ]
        } else {
            [
                (t("welcome.send.s1_title").to_owned(), t("welcome.send.s1_body").to_owned()),
                (t("welcome.send.s2_title").to_owned(), t("welcome.send.s2_body").to_owned()),
                (t("welcome.send.s3_title").to_owned(), t("welcome.send.s3_body").to_owned()),
            ]
        };
        let rx = stage.x + stage.w * 0.54;
        let rw = stage.right() - rx;
        let rh = ((stage.bottom() - y - 2.0 * GAP) / 3.0).clamp(84.0, 108.0);
        for (i, (title, body)) in rows.iter().enumerate() {
            let rect = Rect::new(rx, y + i as f32 * (rh + GAP), rw, rh);
            let e = enter(pt, 0.45 + i as f32 * 0.14, 0.6);
            let on = f32::from(u8::from(active == i));
            let glow = self.anim((Id::Modal, 10 + i as u8), on, 8.0);
            self.gfx.push_layer(e, 30.0 * (1.0 - e));
            self.card(rect);
            if glow > 0.02 {
                self.gfx.stroke(rect, 6.0, 2.0, th.accent.alpha(0.85 * glow));
            }
            let cy = rect.y + rect.h / 2.0;
            self.gfx.circle(rect.x + 36.0, cy, 18.0, th.accent.alpha(0.22 + 0.78 * glow));
            let number_color = th.accent.mix(th.text_on_accent, glow);
            self.gfx.text(&(i + 1).to_string(), Font::BodyStrong, Rect::new(rect.x + 18.0, cy - 12.0, 36.0, 24.0), number_color, Align::Center);
            let (tx, tw) = (rect.x + 72.0, rect.w - 92.0);
            self.gfx.text(title, Font::BodyStrong, Rect::new(tx, rect.y + 14.0, tw, 22.0), th.text, Align::Start);
            let (_, bh) = self.gfx.measure(body, Font::Body, tw, true);
            self.gfx.paragraph(body, Font::Body, Rect::new(tx, rect.y + 38.0, tw, bh + 4.0), th.text_secondary);
            self.gfx.pop_layer();
        }
    }

    // A drawing of the mirror window with its floating bar appearing and disappearing, and the
    // shortcut keys lighting up one after another; the other tips as cards beside it.
    fn page_tips(&mut self, stage: Rect, pt: f32, time: f32) {
        let th = self.theme;
        let y = self.heading(stage.x, stage.y, stage.w, t("welcome.tips.title"), t("welcome.tips.subtitle"), pt);
        let e = enter(pt, 0.3, 0.8);
        let win_w = stage.w * 0.46;
        let win = Rect::new(stage.x, y + 6.0, win_w, (win_w * 0.62).min(stage.bottom() - y - 90.0));
        self.gfx.push_layer(e, 24.0 * (1.0 - e));
        self.shadow(win, 10.0, 26.0);
        self.gfx.fill(win, 10.0, if th.dark { Color::rgb(0x16, 0x16, 0x16) } else { Color::rgb(0x2A, 0x2A, 0x2A) });
        self.gfx.stroke(win, 10.0, 1.5, th.text.alpha(0.35));
        // A phone screen mirrored in the middle, with the sheen that sweeps across it.
        let phone = Rect::new(win.x + win.w / 2.0 - win.h * 0.19, win.y + 12.0, win.h * 0.38, win.h - 24.0);
        self.gfx.gradient(phone, 10.0, brand(AMBER).mix(th.accent, 0.5), brand(PINK).alpha(0.9));
        self.gfx.push_clip(phone);
        let sweep = (time * 0.35).rem_euclid(1.0) * 1.6 - 0.3;
        self.gfx.glow_ellipse(phone.x + phone.w * sweep, phone.y + phone.h / 2.0, phone.w * 0.3, phone.h * 0.8, Color::rgb(255, 255, 255).alpha(0.28));
        self.gfx.pop_clip();
        // The floating bar fades in and out (the mouse "arrives" and "leaves").
        let cycle = (time / 4.0).rem_euclid(1.0);
        let shown = if cycle < 0.15 {
            cycle / 0.15
        } else if cycle < 0.70 {
            1.0
        } else if cycle < 0.85 {
            1.0 - (cycle - 0.70) / 0.15
        } else {
            0.0
        };
        if shown > 0.01 {
            let bar = Rect::new(win.x + win.w / 2.0 - 78.0, win.bottom() - 52.0 + 8.0 * (1.0 - shown), 156.0, 38.0);
            self.gfx.push_layer(shown, 0.0);
            self.gfx.fill(bar, 19.0, Color::rgb(0x20, 0x20, 0x20).alpha(0.92));
            self.gfx.stroke(bar, 19.0, 1.0, Color::rgb(255, 255, 255).alpha(0.18));
            for (i, g) in [icon::FULLSCREEN, icon::RECORD, icon::CAMERA, icon::STOP].into_iter().enumerate() {
                let c = if i == 1 { brand((0xFF, 0x45, 0x45)) } else { Color::rgb(255, 255, 255) };
                self.gfx.glyph(g, Font::IconSmall, bar.x + 24.0 + i as f32 * 36.0, bar.y + 19.0, c);
            }
            self.gfx.pop_layer();
        }
        self.gfx.pop_layer();

        // Shortcut keys under the window; one is "pressed" every second.
        let keys = ["F", "R", "S", "M", "C"];
        let pressed = (time as usize) % keys.len();
        let ky = win.bottom() + 26.0;
        for (i, key) in keys.iter().enumerate() {
            let appear = enter(pt, 0.7 + i as f32 * 0.08, 0.45);
            let down = if i == pressed { 1.0 - (time.rem_euclid(1.0) * 2.0).min(1.0) } else { 0.0 };
            let rect = Rect::new(win.x + 6.0 + i as f32 * 56.0, ky + 3.0 * down, 46.0, 40.0);
            self.gfx.push_layer(appear, 12.0 * (1.0 - appear));
            self.gfx.fill(Rect::new(rect.x, rect.y + 4.0 - 3.0 * down, rect.w, rect.h), 8.0, th.text.alpha(0.10));
            self.gfx.fill(rect, 8.0, th.card.mix(th.accent, 0.35 * down));
            self.gfx.stroke(rect, 8.0, 1.0, th.text.alpha(0.35).mix(th.accent, down));
            self.gfx.text(key, Font::BodyStrong, rect, th.text, Align::Center);
            self.gfx.pop_layer();
        }

        // Tip cards on the right.
        let tips = [
            (icon::TOUCH, "welcome.tip.bar.title", "welcome.tip.bar.body"),
            (icon::KEYBOARD, "welcome.tip.keys.title", "welcome.tip.keys.body"),
            (icon::SETTINGS, "welcome.tip.tray.title", "welcome.tip.tray.body"),
            (icon::WAVE, "welcome.tip.help.title", "welcome.tip.help.body"),
        ];
        let rx = stage.x + stage.w * 0.50;
        let rw = stage.right() - rx;
        let rh = ((stage.bottom() - y - 3.0 * GAP) / 4.0).clamp(72.0, 92.0);
        for (i, (g, title, body)) in tips.into_iter().enumerate() {
            let rect = Rect::new(rx, y + i as f32 * (rh + GAP), rw, rh);
            let e = enter(pt, 0.45 + i as f32 * 0.12, 0.6);
            self.gfx.push_layer(e, 30.0 * (1.0 - e));
            self.card(rect);
            let tile = Rect::new(rect.x + 16.0, rect.y + (rh - 40.0) / 2.0, 40.0, 40.0);
            self.gfx.fill(tile, 10.0, th.accent.alpha(0.16));
            self.gfx.glyph(g, Font::Icon20, tile.x + 20.0, tile.y + 20.0, th.accent);
            let (tx, tw) = (tile.right() + 16.0, rect.right() - tile.right() - 32.0);
            self.gfx.text(t(title), Font::BodyStrong, Rect::new(tx, rect.y + 10.0, tw, 22.0), th.text, Align::Start);
            let (_, bh) = self.gfx.measure(t(body), Font::Body, tw, true);
            self.gfx.paragraph(t(body), Font::Body, Rect::new(tx, rect.y + 34.0, tw, bh + 4.0), th.text_secondary);
            self.gfx.pop_layer();
        }
    }

    fn page_done(&mut self, stage: Rect, pt: f32) {
        let th = self.theme;
        let cx = stage.x + stage.w / 2.0;
        let cy = stage.y + stage.h * 0.30;
        // Confetti for the first seconds.
        if pt < 3.5 && !th.high_contrast {
            let palette = [th.accent, brand(PINK), brand(AMBER), Color::rgb(255, 255, 255)];
            for i in 0..70u32 {
                let delay = 0.1 * rand(i, 6);
                let age = (pt - delay).max(0.0);
                let fall = 90.0 + 160.0 * rand(i, 7);
                let x = cx + (rand(i, 8) - 0.5) * stage.w * 0.95 + 30.0 * (age * 2.0 + i as f32).sin();
                let y = stage.y - 10.0 + age * fall;
                let alpha = (1.0 - age / 3.2).clamp(0.0, 1.0);
                let size = 4.0 + 5.0 * rand(i, 9);
                self.gfx.fill(Rect::new(x, y, size, size * 0.6), 1.5, palette[(i % 4) as usize].alpha(alpha));
            }
        }
        // The disc pops in and the check is drawn stroke by stroke.
        let pop = ease_out_back(pt / 0.6).max(0.0);
        self.gfx.glow(cx, cy, 130.0, th.success.alpha(0.22 * clamp01(pt / 0.6)));
        self.gfx.circle(cx, cy, 46.0 * pop, th.success.alpha(0.18));
        let r = 46.0 * pop;
        self.gfx.stroke(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r), r, 2.0, th.success.alpha(0.9));
        let draw = clamp01((pt - 0.35) / 0.5);
        let (a, b, c) = ((cx - 19.0, cy + 2.0), (cx - 6.0, cy + 15.0), (cx + 21.0, cy - 14.0));
        let first = clamp01(draw / 0.4);
        self.gfx.line(a.0, a.1, a.0 + (b.0 - a.0) * first, a.1 + (b.1 - a.1) * first, 6.0, th.success);
        if draw > 0.4 {
            let second = clamp01((draw - 0.4) / 0.6);
            self.gfx.line(b.0, b.1, b.0 + (c.0 - b.0) * second, b.1 + (c.1 - b.1) * second, 6.0, th.success);
        }
        self.rise(pt, 0.6, |app| app.gfx.text(t("welcome.done.title"), Font::Display, Rect::new(stage.x, cy + 70.0, stage.w, 52.0), th.text, Align::Center));
        let bw = (stage.w - 200.0).min(640.0);
        for (i, key) in ["welcome.done.body", "welcome.done.check"].into_iter().enumerate() {
            let body = t(key);
            let (_, bh) = self.gfx.measure(body, Font::BodyLarge, bw, true);
            let rect = Rect::new(cx - bw / 2.0, cy + 138.0 + i as f32 * 64.0, bw, bh + 4.0);
            self.rise(pt, 0.85 + i as f32 * 0.15, |app| {
                app.gfx.paragraph(body, Font::BodyLarge, rect, if i == 0 { th.text } else { th.text_secondary });
            });
        }
    }
}

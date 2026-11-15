use super::{App, ComboKind, Exp, Id, glyph};
use crate::i18n::{t, tf};
use crate::settings::{ACCENT_KEYS, ACCESS_KEYS, LANGUAGE_KEYS, LATENCY_KEYS, QUALITY_KEYS, RECV_SIZE_NAMES, RESOLUTION_KEYS, KEEP_KEYS, SCALING_KEYS, THEME_KEYS};
use crate::gfx::{Align, Color, Font, Rect};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Style {
    Standard,
    Accent,
    Subtle,
}

impl App {
    pub(super) fn region(&mut self, rect: Rect, id: Id) {
        let rect = match self.clip {
            Some(clip) => match rect.intersect(&clip) {
                Some(r) => r,
                None => return,
            },
            None => rect,
        };
        self.regions.push((rect, id));
    }

    pub(super) fn anim(&mut self, key: (Id, u8), target: f32, rate: f32) -> f32 {
        let value = self.anims.entry(key).or_insert(target);
        let k = 1.0 - (-self.dt * rate).exp();
        *value += (target - *value) * k;
        if (target - *value).abs() < 0.003 {
            *value = target;
        } else {
            self.animating = true;
        }
        *value
    }

    pub(super) fn interact(&mut self, id: Id, rect: Rect, enabled: bool) -> (f32, bool) {
        // Inside an inert block (receiver off) nothing reacts to the pointer or the keyboard.
        let enabled = enabled && !self.inert;
        if enabled {
            self.region(rect, id);
            self.note_focusable(rect, id);
        }
        let hovered = enabled && self.hover == Some(id);
        let pressed = hovered && self.pressed == Some(id);
        (self.anim((id, 0), if hovered { 1.0 } else { 0.0 }, 24.0), pressed)
    }

    // Contrast themes have no tints, so hover shows as a highlight-coloured outline.
    pub(super) fn hc_hover(&self, rect: Rect, radius: f32, hover: f32) {
        if self.theme.high_contrast && hover > 0.5 {
            self.gfx.stroke(rect, radius, 2.0, self.theme.accent);
        }
    }

    pub(super) fn card(&self, rect: Rect) {
        self.gfx.fill(rect, 6.0, self.theme.card);
        self.gfx.stroke(rect, 6.0, 1.0, self.theme.card_stroke);
    }

    // A card that reacts to the pointer; returns its hover amount.
    pub(super) fn card_interactive(&mut self, id: Id, rect: Rect, enabled: bool) -> f32 {
        let (hover, pressed) = self.interact(id, rect, enabled);
        let th = self.theme;
        let fill = if pressed { th.card } else { th.card.mix(th.card_hover, hover) };
        self.gfx.fill(rect, 6.0, fill);
        self.gfx.stroke(rect, 6.0, 1.0, th.card_stroke);
        self.hc_hover(rect, 6.0, hover);
        hover
    }

    pub(super) fn button_width(&self, label: &str, icon: Option<char>) -> f32 {
        let (w, _) = self.gfx.measure(label, Font::Body, 1000.0, false);
        let icon_w = match (icon, label.is_empty()) {
            (None, _) => 0.0,
            (Some(_), true) => 16.0,
            (Some(_), false) => 24.0,
        };
        (w + icon_w + 24.0).max(if label.is_empty() { 32.0 } else { 96.0 })
    }

    pub(super) fn button(&mut self, id: Id, rect: Rect, label: &str, icon: Option<char>, style: Style, enabled: bool) {
        let (h, pressed) = self.interact(id, rect, enabled);
        let th = self.theme;
        let (fill, text) = match style {
            Style::Accent if enabled => {
                let a = if pressed { 0.8 } else { 1.0 - 0.1 * h };
                (th.accent.alpha(a), if pressed { th.text_on_accent.alpha(0.78) } else { th.text_on_accent })
            }
            Style::Accent => (
                if th.dark { Color::argb(0x28FF_FFFF) } else { Color::argb(0x3700_0000) },
                if th.dark { Color::argb(0x87FF_FFFF) } else { Color::argb(0xFFFF_FFFF) },
            ),
            Style::Standard if enabled => {
                let fill = if pressed { th.control_pressed } else { th.control.mix(th.control_hover, h) };
                (fill, if pressed { th.text_secondary } else { th.text })
            }
            Style::Standard => (th.control_disabled, th.text_disabled),
            Style::Subtle => {
                let fill = if pressed { th.subtle_pressed } else { th.subtle_hover.alpha(h) };
                (fill, if !enabled { th.text_disabled } else if pressed { th.text_secondary } else { th.text })
            }
        };
        self.gfx.fill(rect, 4.0, fill);
        match style {
            Style::Standard => {
                self.gfx.stroke(rect, 4.0, 1.0, th.control_stroke);
                if enabled && !pressed {
                    self.gfx.line(rect.x + 4.0, rect.bottom() - 0.5, rect.right() - 4.0, rect.bottom() - 0.5, 1.0, th.control_stroke_bottom);
                }
            }
            Style::Accent if enabled => {
                self.gfx.stroke(rect, 4.0, 1.0, Color::argb(0x1400_0000));
                self.gfx.line(rect.x + 4.0, rect.bottom() - 0.5, rect.right() - 4.0, rect.bottom() - 0.5, 1.0, Color::argb(0x2900_0000));
            }
            _ => {}
        }
        self.hc_hover(rect, 4.0, h);
        let (label_w, _) = if label.is_empty() { (0.0, 0.0) } else { self.gfx.measure(label, Font::Body, 1000.0, false) };
        let icon_w = match (icon, label.is_empty()) {
            (None, _) => 0.0,
            (Some(_), true) => 16.0,
            (Some(_), false) => 24.0,
        };
        let mut x = rect.x + (rect.w - label_w - icon_w) / 2.0;
        let cy = rect.y + rect.h / 2.0;
        if let Some(icon) = icon {
            self.gfx.glyph(icon, Font::Icon, x + 8.0, cy, text);
            x += icon_w;
        }
        self.gfx.text(label, Font::Body, Rect::new(x, rect.y, label_w + 2.0, rect.h - 1.0), text, Align::Start);
    }

    pub(super) fn toggle(&mut self, id: Id, x: f32, cy: f32, on: bool, enabled: bool) {
        let rect = Rect::new(x, cy - 10.0, 40.0, 20.0);
        let (h, pressed) = self.interact(id, rect.inset(-4.0, -6.0), enabled);
        let pos = self.anim((id, 1), if on { 1.0 } else { 0.0 }, 16.0);
        let press = self.anim((id, 2), if pressed { 1.0 } else { 0.0 }, 30.0);
        let th = self.theme;
        let fade = if enabled { 1.0 } else { 0.45 };
        let off_fill = th.control_pressed.mix(th.control_hover, h);
        self.gfx.fill(rect, 10.0, off_fill.alpha((1.0 - pos) * fade));
        self.gfx.stroke(rect, 10.0, 1.0, th.control_strong.alpha((1.0 - pos) * fade));
        let on_fill = th.accent.alpha(1.0 - 0.1 * h - 0.1 * press);
        self.gfx.fill(rect, 10.0, on_fill.alpha(pos * if enabled { 1.0 } else { 0.4 }));
        let radius = 6.0 + h - press * 0.5;
        let knob_w = press * 3.0;
        let left = rect.x + 10.0;
        let right = rect.right() - 10.0 - knob_w;
        let eased = pos * pos * (3.0 - 2.0 * pos);
        let kx = left + (right - left) * eased;
        let knob = th.control_strong.mix(th.text_on_accent, pos).alpha(fade);
        self.gfx.fill(Rect::new(kx - radius, cy - radius, radius * 2.0 + knob_w, radius * 2.0), radius, knob);
    }

    pub(super) fn toggle_with_label(&mut self, id: Id, right: f32, cy: f32, on: bool, enabled: bool) {
        let label = t(if on { "common.on" } else { "common.off" });
        let color = if enabled { self.theme.text } else { self.theme.text_disabled };
        self.gfx.text(label, Font::Body, Rect::new(right - 40.0 - 112.0, cy - 10.0, 100.0, 20.0), color, Align::End);
        self.toggle(id, right - 40.0, cy, on, enabled);
    }

    pub(super) fn combo_box(&mut self, kind: ComboKind, rect: Rect, label: &str, enabled: bool) {
        let id = Id::Combo(kind);
        let (h, pressed) = self.interact(id, rect, enabled);
        let th = self.theme;
        let open = self.combo == Some(kind);
        let fill = if !enabled {
            th.control_disabled
        } else if pressed || open {
            th.control_pressed
        } else {
            th.control.mix(th.control_hover, h)
        };
        self.gfx.fill(rect, 4.0, fill);
        self.gfx.stroke(rect, 4.0, 1.0, th.control_stroke);
        if enabled {
            self.gfx.line(rect.x + 4.0, rect.bottom() - 0.5, rect.right() - 4.0, rect.bottom() - 0.5, 1.0, th.control_stroke_bottom);
        }
        self.hc_hover(rect, 4.0, h);
        let color = if enabled { th.text } else { th.text_disabled };
        let mut text_x = rect.x + 12.0;
        if kind == ComboKind::Window {
            let icon = self.selected_window_index().and_then(|i| self.windows[i].icon.clone());
            self.window_icon(icon.as_ref(), rect.x + 12.0, rect.y + rect.h / 2.0, enabled);
            text_x += 26.0;
        }
        self.gfx.text(label, Font::Body, Rect::new(text_x, rect.y, rect.x + rect.w - 44.0 - text_x, rect.h - 1.0), color, Align::Start);
        let drop = if pressed { 2.0 } else { 0.0 };
        self.gfx.glyph(
            glyph::CHEVRON_DOWN,
            Font::IconSmall,
            rect.right() - 20.0,
            rect.y + rect.h / 2.0 + drop,
            if enabled { th.text_secondary } else { th.text_disabled },
        );
        if open {
            self.combo_anchor = rect;
        }
    }

    // A window's icon, or a generic window glyph when it has none.
    pub(super) fn window_icon(&self, icon: Option<&ID2D1Bitmap1>, x: f32, cy: f32, enabled: bool) {
        match icon {
            Some(bitmap) => self.gfx.image(bitmap, Rect::new(x, cy - 8.0, 16.0, 16.0), if enabled { 1.0 } else { 0.5 }),
            None => self.gfx.glyph(glyph::WINDOW, Font::IconSmall, x + 8.0, cy, self.theme.text_secondary),
        }
    }

    pub(super) fn combo_items(&self, kind: ComboKind) -> Vec<String> {
        match kind {
            ComboKind::Source => vec![t("send.source.screen").to_owned(), t("send.source.window").to_owned()],
            ComboKind::Window if self.windows.is_empty() => vec![t("send.window.none").to_owned()],
            ComboKind::Window => self
                .windows
                .iter()
                .map(|w| if w.process.is_empty() { w.title.clone() } else { format!("{}  ·  {}", w.title, w.process) })
                .collect(),
            ComboKind::Monitor if self.monitors.is_empty() => vec![t("send.settings.display_default").to_owned()],
            ComboKind::Monitor => self
                .monitors
                .iter()
                .map(|m| {
                    let name = m.name.clone().unwrap_or_else(|| tf("send.settings.display_name", &[("number", &(m.index + 1))]));
                    format!("{name}  ·  {} × {}", m.width, m.height)
                })
                .collect(),
            ComboKind::Fps | ComboKind::RecvFps => vec![String::from("30 fps"), String::from("60 fps")],
            ComboKind::Quality => QUALITY_KEYS
                .iter()
                .zip(crate::settings::QUALITY_BITRATES)
                .map(|(key, bitrate)| format!("{}  ·  {} Mbps", t(key), bitrate / 1_000_000))
                .collect(),
            ComboKind::Resolution => RESOLUTION_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::AudioLatency => LATENCY_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::RecvSize => RECV_SIZE_NAMES.iter().map(|s| (*s).to_owned()).collect(),
            ComboKind::RecvScaling => SCALING_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::RecvKeep => KEEP_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::RecvAccess => ACCESS_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::Theme => THEME_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::Accent => ACCENT_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::Language => LANGUAGE_KEYS.iter().map(|key| t(key).to_owned()).collect(),
            ComboKind::RecvOutput => self.output_items(),
        }
    }

    pub(super) fn combo_selected(&self, kind: ComboKind) -> usize {
        match kind {
            ComboKind::Monitor => self.settings.monitor as usize,
            ComboKind::Source => usize::from(self.settings.share_window),
            ComboKind::Window => self.selected_window_index().unwrap_or(usize::MAX),
            ComboKind::Fps => usize::from(self.settings.fps >= 60),
            ComboKind::RecvFps => usize::from(self.settings.receiver_fps >= 60),
            ComboKind::Quality => self.settings.quality,
            ComboKind::Resolution => self.settings.resolution,
            ComboKind::AudioLatency => self.settings.audio_latency,
            ComboKind::RecvSize => self.settings.receiver_size,
            ComboKind::RecvScaling => self.settings.receiver_scaling,
            ComboKind::RecvKeep => self.settings.receiver_keep,
            ComboKind::RecvAccess => self.settings.access_policy,
            ComboKind::Theme => self.settings.theme,
            ComboKind::Accent => self.settings.accent,
            ComboKind::Language => self.settings.language,
            ComboKind::RecvOutput => self.output_selected(),
        }
    }

    pub(super) fn combo_label(&self, kind: ComboKind) -> String {
        if kind == ComboKind::Window {
            let title = self.settings.window_title.trim();
            return if title.is_empty() { t("send.window.choose").to_owned() } else { title.to_owned() };
        }
        self.combo_items(kind).get(self.combo_selected(kind)).cloned().unwrap_or_default()
    }

    // Expandable settings card (WinUI SettingsExpander). Returns the body rect and the open amount;
    // the caller draws `rows` child rows of CHILD_H inside it with `child_row`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn expander(
        &mut self,
        kind: Exp,
        rect: Rect,
        icon: char,
        title: &str,
        description: &str,
        summary: &str,
        rows: usize,
        header_control_w: f32,
    ) -> (Rect, f32) {
        let open = self.expanded.contains(&kind);
        let th = self.theme;
        let amount = self.anim((Id::Expander(kind), 1), if open { 1.0 } else { 0.0 }, 18.0);
        let eased = 1.0 - (1.0 - amount).powi(3);
        let body_h = rows as f32 * CHILD_H * eased;
        let whole = Rect::new(rect.x, rect.y, rect.w, rect.h + body_h);
        let (hover, pressed) = self.interact(Id::Expander(kind), rect, true);
        self.gfx.fill(whole, 6.0, th.card);
        if body_h > 0.5 {
            self.gfx.push_clip(Rect::new(whole.x, rect.bottom(), whole.w, body_h));
            self.gfx.fill(whole, 6.0, th.card_secondary);
            self.gfx.pop_clip();
            self.gfx.line(rect.x, rect.bottom(), rect.right(), rect.bottom(), 1.0, th.card_stroke);
        }
        let header_fill = if pressed { th.subtle_pressed } else { th.subtle_hover.alpha(hover * 0.6) };
        self.gfx.push_clip(rect);
        self.gfx.fill(Rect::new(rect.x, rect.y, rect.w, rect.h + if body_h > 0.5 { 6.0 } else { 0.0 }), 6.0, header_fill);
        self.gfx.pop_clip();
        self.gfx.stroke(whole, 6.0, 1.0, th.card_stroke);
        self.hc_hover(rect, 6.0, hover);
        let reserved = header_control_w + if summary.is_empty() { 24.0 } else { 300.0 };
        self.row_content(rect, icon, title, description, reserved, true);
        let chevron_x = rect.right() - 28.0;
        let cy = rect.y + rect.h / 2.0;
        let chevron = if eased > 0.5 { glyph::CHEVRON_UP } else { glyph::CHEVRON_DOWN };
        self.gfx.glyph(chevron, Font::IconSmall, chevron_x, cy, th.text_secondary);
        if !summary.is_empty() {
            let w = 280.0;
            let right = chevron_x - 24.0 - header_control_w;
            self.gfx.text(summary, Font::Body, Rect::new(right - w, cy - 10.0, w, 20.0), th.text_secondary, Align::End);
        }
        (Rect::new(rect.x, rect.bottom(), rect.w, body_h), eased)
    }

    // A row inside an expander body; returns its rect for placing the control.
    pub(super) fn child_row(&self, body: Rect, index: usize, title: &str, description: &str, enabled: bool) -> Rect {
        let th = self.theme;
        let rect = Rect::new(body.x, body.y + index as f32 * CHILD_H, body.w, CHILD_H);
        if index > 0 {
            self.gfx.line(rect.x + 1.0, rect.y, rect.right() - 1.0, rect.y, 1.0, th.card_stroke);
        }
        let (primary, secondary) = if enabled { (th.text, th.text_secondary) } else { (th.text_disabled, th.text_disabled) };
        let x = rect.x + 60.0;
        let w = rect.w - 60.0 - 300.0;
        if description.is_empty() {
            self.gfx.text(title, Font::Body, Rect::new(x, rect.y, w, rect.h), primary, Align::Start);
        } else {
            let cy = rect.y + rect.h / 2.0;
            self.gfx.text(title, Font::Body, Rect::new(x, cy - 19.0, w, 20.0), primary, Align::Start);
            self.gfx.text(description, Font::Caption, Rect::new(x, cy + 1.0, w, 18.0), secondary, Align::Start);
        }
        rect
    }

    // Runs `draw` with drawing and hit regions clipped to `area`.
    pub(super) fn clipped(&mut self, area: Rect, draw: impl FnOnce(&mut Self)) {
        let previous = self.clip;
        let clip = match previous {
            Some(p) => p.intersect(&area),
            None => Some(area),
        };
        let Some(clip) = clip else { return };
        self.gfx.push_clip(clip);
        self.clip = Some(clip);
        draw(self);
        self.clip = previous;
        self.gfx.pop_clip();
    }

    pub(super) fn spinner(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        self.animating = true;
        let time = self.started.elapsed().as_secs_f32();
        let start = time * std::f32::consts::TAU * 0.85;
        let sweep = 0.35 + 3.6 * (0.5 - 0.5 * (time * 2.4).cos());
        self.gfx.arc(cx, cy, r, start, sweep, 2.0 + r / 10.0, color);
    }

    // Soft drop shadow built from many faint rounded rects, so no individual step shows.
    // Draw it inside the same layer as the surface it belongs to, so both fade together.
    pub(super) fn shadow(&self, rect: Rect, radius: f32, depth: f32) {
        if depth <= 0.5 {
            return;
        }
        let steps = (depth * 1.5).clamp(8.0, 32.0) as usize;
        let strength = if self.theme.dark { 0.30 } else { 0.14 };
        let offset = depth * 0.3;
        for i in 0..steps {
            let f = (i as f32 + 0.5) / steps as f32;
            let spread = depth * f;
            let a = strength * 2.0 * (1.0 - f) / steps as f32;
            let r = Rect::new(rect.x - spread, rect.y - spread + offset, rect.w + 2.0 * spread, rect.h + 2.0 * spread);
            self.gfx.fill(r, radius + spread, Color::rgba(0, 0, 0, 255).alpha(a));
        }
    }

    pub(super) fn text_box(&mut self, id: Id, rect: Rect, text: &str, enabled: bool) {
        let (h, _) = self.interact(id, rect, enabled);
        let th = self.theme;
        let focused = self.focus == Some(id);
        let fill = if !enabled {
            th.control_disabled
        } else if focused {
            th.input_active
        } else {
            th.control.mix(th.control_hover, h)
        };
        self.gfx.fill(rect, 4.0, fill);
        self.gfx.stroke(rect, 4.0, 1.0, th.control_stroke);
        self.gfx.push_clip(Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 1.0));
        if focused {
            self.gfx.fill(Rect::new(rect.x, rect.bottom() - 2.0, rect.w, 2.0), 0.0, th.accent);
        } else if enabled {
            self.gfx.fill(Rect::new(rect.x, rect.bottom() - 1.0, rect.w, 1.0), 0.0, th.control_strong);
        }
        self.gfx.pop_clip();
        let color = if enabled { th.text } else { th.text_disabled };
        let inner = Rect::new(rect.x + 11.0, rect.y, rect.w - 22.0, rect.h - 1.0);
        self.gfx.text(text, Font::Body, inner, color, Align::Start);
        if focused && self.caret_visible() {
            let (w, _) = self.gfx.measure(text, Font::Body, 10_000.0, false);
            let x = (inner.x + w + 1.0).min(inner.right());
            self.gfx.line(x, rect.y + 8.0, x, rect.bottom() - 9.0, 1.0, th.text);
        }
    }

    pub(super) fn caret_visible(&self) -> bool {
        (self.caret_epoch.elapsed().as_millis() / 530) % 2 == 0
    }

    // Settings-style row: icon, title and description on the left, a control on the right.
    pub(super) fn row_content(&self, rect: Rect, icon: char, title: &str, description: &str, control_w: f32, enabled: bool) {
        let th = self.theme;
        let (primary, secondary) = if enabled { (th.text, th.text_secondary) } else { (th.text_disabled, th.text_disabled) };
        self.gfx.glyph(icon, Font::Icon20, rect.x + 30.0, rect.y + rect.h / 2.0, primary);
        let x = rect.x + 60.0;
        let w = rect.w - 60.0 - control_w - 32.0;
        if description.is_empty() {
            self.gfx.text(title, Font::Body, Rect::new(x, rect.y, w, rect.h), primary, Align::Start);
        } else {
            let cy = rect.y + rect.h / 2.0;
            self.gfx.text(title, Font::Body, Rect::new(x, cy - 20.0, w, 20.0), primary, Align::Start);
            self.gfx.text(description, Font::Caption, Rect::new(x, cy, w, 18.0), secondary, Align::Start);
        }
    }

    pub(super) fn row(&self, rect: Rect, icon: char, title: &str, description: &str, control_w: f32, enabled: bool) -> Rect {
        self.card(rect);
        self.row_content(rect, icon, title, description, control_w, enabled);
        rect
    }

    pub(super) fn section(&self, x: f32, y: f32, w: f32, title: &str) {
        self.gfx.text(title, Font::BodyStrong, Rect::new(x + 2.0, y, w, 20.0), self.theme.text, Align::Start);
    }

    pub(super) fn badge_width(&self, text: &str) -> f32 {
        self.gfx.measure(text, Font::Caption, 400.0, false).0 + 30.0
    }

    // A status pill such as «● Activo».
    pub(super) fn badge(&self, x: f32, cy: f32, text: &str, color: Color) -> f32 {
        let (tw, _) = self.gfx.measure(text, Font::Caption, 400.0, false);
        let rect = Rect::new(x, cy - 12.0, tw + 30.0, 24.0);
        self.gfx.fill(rect, 12.0, color.alpha(0.16));
        self.gfx.circle(rect.x + 12.0, cy, 3.5, color);
        self.gfx.text(text, Font::Caption, Rect::new(rect.x + 21.0, rect.y, tw + 4.0, rect.h), self.theme.text, Align::Start);
        rect.w
    }

    // Illustration of this PC and the other device, drawn as real devices with lit screens. Alone,
    // the PC sits in the middle (dark when off, sending AirPlay-like waves while it waits); once a
    // peer joins, the PC moves aside, the peer slides in and comets of light travel along an arc
    // between them, always from the sender (left) to the receiver (right).
    pub(super) fn link_art(&mut self, rect: Rect, home: char, peer: char, home_left: bool, state: Link, slot: u8) {
        let th = self.theme;
        let cy = rect.y + rect.h / 2.0 + 4.0;
        let left = rect.x + 58.0;
        let right = rect.right() - 58.0;
        let center = rect.x + rect.w / 2.0;
        let time = self.started.elapsed().as_secs_f32();
        let together = matches!(state, Link::Connecting | Link::Flowing);
        let p = self.anim((Id::LinkArt, slot), if together { 1.0 } else { 0.0 }, 6.0);
        let e = p * p * (3.0 - 2.0 * p);
        let lit = self.anim((Id::LinkArt, slot + 8), if state == Link::Off { 0.0 } else { 1.0 }, 6.0);
        let flow = self.anim((Id::LinkArt, slot + 24), if state == Link::Flowing { 1.0 } else { 0.0 }, 4.0);
        let (home_side, peer_side) = if home_left { (left, right) } else { (right, left) };
        let home_x = center + (home_side - center) * e;
        let peer_x = peer_side + (if home_left { 40.0 } else { -40.0 }) * (1.0 - e);

        // Waiting alone: waves rise from the PC like the AirPlay symbol.
        let waves = self.anim((Id::LinkArt, slot + 16), if state == Link::Waiting { 1.0 } else { 0.0 }, 5.0) * (1.0 - e);
        if waves > 0.01 && !th.high_contrast {
            let origin = cy + 4.0;
            for k in 0..3 {
                let phase = (time * 0.38 + k as f32 / 3.0).rem_euclid(1.0);
                let r = 52.0 + 34.0 * phase;
                let alpha = 0.8 * (1.0 - phase).powf(1.4) * phase.min(0.15) / 0.15 * waves;
                let sweep = 1.25;
                self.gfx.arc(home_x, origin, r, -std::f32::consts::FRAC_PI_2 - sweep / 2.0, sweep, 2.5, th.accent.alpha(alpha));
            }
        }

        // The link: a soft arc between both devices with comets of light along it.
        if e > 0.01 {
            let (a, b) = if home_x < peer_x { (home_x, peer_x) } else { (peer_x, home_x) };
            let p0 = (a + 52.0, cy - 6.0);
            let p2 = (b - 52.0, cy - 6.0);
            let p1 = ((p0.0 + p2.0) / 2.0, cy - 70.0);
            let base = if state == Link::Connecting {
                // While connecting the arc draws itself again and again.
                let grow = (time * 0.9).rem_euclid(1.0);
                self.gfx.curve(p0, p1, p2, 0.0, grow, 2.5, th.accent.alpha(0.8 * e));
                0.25
            } else {
                0.45
            };
            self.gfx.curve(p0, p1, p2, 0.0, 1.0, 2.0, th.accent.alpha(base * e));
            if flow > 0.01 {
                for k in 0..3 {
                    let head = (time * 0.42 + k as f32 / 3.0).rem_euclid(1.0);
                    for s in 0..12 {
                        let t = head - s as f32 * 0.016;
                        if t < 0.0 {
                            break;
                        }
                        let (x, y) = crate::gfx::Gfx::curve_point(p0, p1, p2, t);
                        let fade = 1.0 - s as f32 / 12.0;
                        let edge = (t.min(1.0 - t) * 8.0).min(1.0);
                        let core = if s == 0 { Color::rgb(255, 255, 255).mix(th.accent, 0.35) } else { th.accent };
                        self.gfx.circle(x, y, 1.5 + 3.0 * fade, core.alpha(fade * edge * flow * e));
                    }
                }
            }
            self.gfx.push_layer(e, 0.0);
            self.device(peer, peer_x, cy, lit, flow, time, 0.5);
            self.gfx.pop_layer();
        }
        self.device(home, home_x, cy, lit, flow, time, 0.0);
        if (lit > 0.0 && lit < 1.0) || (p > 0.0 && p < 1.0) || (flow > 0.0 && flow < 1.0) {
            self.animating = true;
        } else if state != Link::Off {
            self.ambient = true;
        }
    }

    // One device of the illustration, centred on (cx, cy). `lit` lights its screen; `flow` makes
    // a sheen sweep across it (the mirrored picture); `offset` desynchronises two devices.
    #[allow(clippy::too_many_arguments)]
    fn device(&self, kind: char, cx: f32, cy: f32, lit: f32, flow: f32, time: f32, offset: f32) {
        let th = self.theme;
        let frame = if th.high_contrast { th.text } else { th.text.alpha(if th.dark { 0.55 } else { 0.62 }) };
        let body = if th.dark { Color::rgb(0x1C, 0x1C, 0x1C) } else { Color::rgb(0xFA, 0xFA, 0xFA) };
        let (outer, radius, screen) = if kind == glyph::PHONE {
            let o = Rect::new(cx - 21.0, cy - 40.0, 42.0, 80.0);
            (o, 10.0, o.inset(3.5, 3.5))
        } else if kind == glyph::TV {
            let o = Rect::new(cx - 54.0, cy - 34.0, 108.0, 64.0);
            (o, 5.0, o.inset(4.0, 4.0))
        } else {
            let o = Rect::new(cx - 48.0, cy - 38.0, 96.0, 62.0);
            (o, 7.0, o.inset(5.0, 5.0))
        };
        // Stand or feet under the screen.
        if kind == glyph::PC {
            self.gfx.fill(Rect::new(cx - 9.0, outer.bottom(), 18.0, 11.0), 0.0, frame.alpha(0.6));
            self.gfx.fill(Rect::new(cx - 24.0, outer.bottom() + 10.0, 48.0, 4.0), 2.0, frame);
        } else if kind == glyph::TV {
            self.gfx.line(outer.x + 18.0, outer.bottom(), outer.x + 12.0, outer.bottom() + 8.0, 2.5, frame);
            self.gfx.line(outer.right() - 18.0, outer.bottom(), outer.right() - 12.0, outer.bottom() + 8.0, 2.5, frame);
        }
        if !th.high_contrast {
            self.gfx.glow_ellipse(cx, cy, outer.w * 0.9, outer.h * 0.9, th.accent.alpha(0.10 * lit));
        }
        self.gfx.fill(outer, radius, body);
        self.gfx.stroke(outer, radius, 2.0, frame);
        let inner = (radius - 3.0).max(2.0);
        self.gfx.fill(screen, inner, if th.dark { Color::rgb(0x0E, 0x0E, 0x0E) } else { Color::rgb(0x2A, 0x2A, 0x2A) });
        if lit > 0.01 {
            // RustPlay colours on the screen: accent into a warm pink, like the logo glow.
            let pink = Color::rgb(0xFF, 0x4F, 0x8B);
            let amber = Color::rgb(0xFF, 0xB3, 0x47);
            let level = if th.high_contrast { 1.0 } else { 0.55 + 0.45 * flow };
            let orange = Color::rgb(0xFF, 0x6A, 0x1A);
            self.gfx.gradient(screen, inner, amber.mix(orange, 0.5).alpha(lit * level), pink.alpha(lit * level * 0.9));
            if flow > 0.01 && !th.high_contrast {
                self.gfx.push_clip(screen);
                let sweep = ((time * 0.35 + offset).rem_euclid(1.0)) * 1.6 - 0.3;
                let sx = screen.x + screen.w * sweep;
                self.gfx.glow_ellipse(sx, screen.y + screen.h / 2.0, screen.w * 0.28, screen.h * 1.2, Color::rgb(255, 255, 255).alpha(0.32 * flow));
                self.gfx.pop_clip();
            }
        }
        if kind == glyph::PHONE {
            self.gfx.fill(Rect::new(cx - 7.0, screen.y + 4.0, 14.0, 4.5), 2.25, Color::rgb(0, 0, 0).alpha(0.85));
        }
    }

    pub(super) fn draw_toast(&mut self, w: f32, h: f32) {
        let Some((text, at)) = self.toast.clone() else { return };
        let age = at.elapsed().as_secs_f32();
        if age > 2.6 {
            self.toast = None;
            return;
        }
        let show = self.anim((Id::Toast, 0), if age < 2.2 { 1.0 } else { 0.0 }, 16.0);
        self.animating = true;
        let th = self.theme;
        let (tw, _) = self.gfx.measure(&text, Font::Body, 600.0, false);
        let rect = Rect::new(NAV_CENTER_OFFSET + (w - NAV_CENTER_OFFSET - tw - 64.0) / 2.0, h - 72.0, tw + 64.0, 44.0);
        self.gfx.push_layer(show, 12.0 * (1.0 - show));
        self.shadow(rect, 8.0, 12.0);
        self.gfx.fill(rect, 8.0, th.flyout);
        self.gfx.stroke(rect, 8.0, 1.0, th.flyout_stroke);
        self.gfx.glyph(glyph::SUCCESS, Font::Icon, rect.x + 22.0, rect.y + 22.0, th.success);
        self.gfx.text(&text, Font::Body, Rect::new(rect.x + 40.0, rect.y, tw + 4.0, rect.h), th.text, Align::Start);
        self.gfx.pop_layer();
    }
}

const NAV_CENTER_OFFSET: f32 = super::NAV_W;
pub(super) const CHILD_H: f32 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Link {
    // PC alone, dimmed (receiver off).
    Off,
    // PC alone, ready but not looking for anything.
    Idle,
    // PC alone, sending radar rings (searching or waiting for a device).
    Waiting,
    // Both devices, rings around the peer while the connection is set up.
    Connecting,
    // Both devices with dots flowing between them.
    Flowing,
}

use super::widgets::{Link, Style};
use super::{
    Exp, App, LinkKind, CAPTION_H, CAPTION_W, ComboKind, GAP, Id, MAX_CONTENT, Mirror, NAV_W, PAD, PIN_LEN, Page, ROW_H,
    ReceiverState, TITLE_H, glyph,
};
use crate::gfx::{Align, Color, Font, Rect};
use crate::i18n::{t, tf};

const VIEW_TOP: f32 = TITLE_H + 104.0;
const HERO_H: f32 = 184.0;

impl App {
    pub(super) fn content_frame(&self) -> (f32, f32) {
        let (w, _) = self.gfx.size_dip();
        let width = (w - NAV_W - 2.0 * PAD).min(MAX_CONTENT);
        (NAV_W + PAD, width)
    }

    pub(super) fn draw_title_bar(&mut self, w: f32) {
        let th = self.theme;
        let text = if self.active { th.text } else { th.text_tertiary };
        if let Some(logo) = self.logo.clone() {
            self.gfx.image(&logo, Rect::new(16.0, 16.0, 16.0, 16.0), if self.active { 1.0 } else { 0.6 });
        }
        self.gfx.text("RustPlay", Font::Caption, Rect::new(44.0, 0.0, 200.0, TITLE_H), text, Align::Start);
        let [min, max, close] = self.caption_rects();
        let glyph_color = if self.active { th.text } else { th.text_tertiary };
        let (hover, pressed) = self.interact(Id::Minimize, min, true);
        self.gfx.fill(min, 0.0, if pressed { th.subtle_pressed } else { th.subtle_hover.alpha(hover) });
        self.gfx.glyph(glyph::MINIMIZE, Font::IconCaption, min.x + CAPTION_W / 2.0, CAPTION_H / 2.0, glyph_color);
        let max_fill = if self.max_pressed { th.subtle_pressed } else if self.max_hover { th.subtle_hover } else { Color::TRANSPARENT };
        self.gfx.fill(max, 0.0, max_fill);
        let max_glyph = if self.maximized { glyph::RESTORE } else { glyph::MAXIMIZE };
        self.gfx.glyph(max_glyph, Font::IconCaption, max.x + CAPTION_W / 2.0, CAPTION_H / 2.0, glyph_color);
        let (hover, pressed) = self.interact(Id::Close, close, true);
        let close_fill = th.close_hover.alpha(if pressed { 0.9 * hover } else { hover });
        self.gfx.push_clip(close);
        self.gfx.fill(Rect::new(close.x, close.y - 8.0, close.w + 8.0, close.h + 8.0), if self.maximized { 0.0 } else { 8.0 }, close_fill);
        self.gfx.pop_clip();
        let close_glyph = glyph_color.mix(Color::rgb(255, 255, 255), hover);
        self.gfx.glyph(glyph::CHROME_CLOSE, Font::IconCaption, close.x + CAPTION_W / 2.0, CAPTION_H / 2.0, close_glyph);
        let _ = w;
    }

    pub(super) fn status_line(&self) -> (String, Color) {
        let th = self.theme;
        match (&self.mirror, self.receiver) {
            (Mirror::Active { key, .. }, _) => {
                let name = self.device_name(key).unwrap_or(t("common.the_tv"));
                (tf("status.mirroring", &[("name", &name)]), th.success)
            }
            (Mirror::Connecting { .. }, _) => (t("status.connecting").to_owned(), th.accent),
            (Mirror::Reconnecting { .. }, _) => (t("status.reconnecting").to_owned(), th.accent),
            (_, ReceiverState::On { .. }) if !self.clients.is_empty() => (t("status.receiving").to_owned(), th.success),
            (_, ReceiverState::On { .. }) => (tf("status.visible", &[("name", &self.settings.receiver_name)]), th.success),
            (_, ReceiverState::Starting) => (t("status.starting_receiver").to_owned(), th.accent),
            _ => (t("status.ready").to_owned(), th.text_tertiary),
        }
    }

    pub(super) fn draw_nav(&mut self, h: f32) {
        let th = self.theme;
        let card = Rect::new(12.0, TITLE_H + 4.0, NAV_W - 24.0, 76.0);
        // Entrance: the start-up animation flies the logo here, then the name and each
        // navigation item come in turn.
        if let Some(logo) = self.logo.clone()
            && self.splash_time().is_none()
        {
            let (cx, cy, size) = super::splash::NAV_LOGO;
            self.gfx.image(&logo, Rect::new(cx - size / 2.0, cy - size / 2.0, size, size), 1.0);
        }
        let name = self.intro(0.12, 0.4);
        self.gfx.push_layer(name, 6.0 * (1.0 - name));
        self.gfx.text("RustPlay", Font::Subtitle, Rect::new(card.x + 78.0, card.y + 12.0, card.w - 86.0, 28.0), th.text, Align::Start);
        self.gfx.text(t("app.tagline"), Font::Caption, Rect::new(card.x + 78.0, card.y + 42.0, card.w - 86.0, 20.0), th.text_secondary, Align::Start);
        self.gfx.pop_layer();

        let mut pages = vec![
            (Page::Send, glyph::CAST, t("nav.send")),
            (Page::Receive, glyph::RECEIVE, t("nav.receive")),
            (Page::History, glyph::HISTORY, t("nav.history")),
        ];
        // The log page is a developer option (Ajustes → Desarrollador).
        if self.settings.developer_logs {
            pages.push((Page::Logs, glyph::LOG, t("nav.logs")));
        }
        pages.push((Page::About, glyph::INFO, t("nav.about")));
        let top = TITLE_H + 100.0;
        let mut selected_y = top;
        let count = pages.len();
        let items = pages.into_iter().chain([(Page::Settings, glyph::SETTINGS, t("nav.settings"))]);
        for (i, (page, icon, label)) in items.enumerate() {
            let y = if i < count { top + i as f32 * 40.0 } else { h - 52.0 };
            let rect = Rect::new(8.0, y, NAV_W - 16.0, 36.0);
            let shown = self.intro(0.2 + i as f32 * 0.06, 0.35);
            self.gfx.push_layer(shown, 10.0 * (1.0 - shown));
            let id = Id::Nav(page);
            let (hover, pressed) = self.interact(id, rect, true);
            let selected = self.page == page;
            if selected {
                selected_y = rect.y;
            }
            let base = if selected { th.subtle_hover } else { Color::TRANSPARENT };
            let fill = if pressed { th.subtle_pressed } else { base.mix(th.subtle_hover.alpha(if selected { 1.5 } else { 1.0 }), hover) };
            self.gfx.fill(rect, 4.0, fill);
            let color = if pressed { th.text_secondary } else { th.text };
            self.gfx.glyph(icon, Font::Icon, rect.x + 24.0, rect.y + 18.0, color);
            self.gfx.text(label, Font::Body, Rect::new(rect.x + 48.0, rect.y, rect.w - 56.0, rect.h - 1.0), color, Align::Start);
            let dot = match page {
                Page::Send => matches!(self.mirror, Mirror::Active { .. }),
                Page::Receive => !self.clients.is_empty(),
                _ => false,
            };
            if dot {
                self.gfx.circle(rect.right() - 16.0, rect.y + 18.0, 4.0, th.success);
            }
            self.gfx.pop_layer();
        }
        // The selection pill slides between items and stretches while moving, as in WinUI.
        let target = selected_y + 10.0;
        let y = self.anim((Id::NavIndicator, 0), target, 16.0);
        let stretch = ((target - y).abs() * 0.5).min(24.0);
        let top_y = if target > y { y } else { y - stretch };
        let pill = self.intro(0.5, 0.3);
        self.gfx.fill(Rect::new(8.0, top_y, 3.0, 16.0 + stretch), 1.5, th.accent.alpha(pill));

        self.gfx.line(16.0, h - 62.0, NAV_W - 16.0, h - 62.0, 1.0, th.divider_subtle());
    }

    pub(super) fn draw_page(&mut self, w: f32, h: f32) {
        let th = self.theme;
        let (x, width) = self.content_frame();
        let (title, subtitle) = match self.page {
            Page::Send => (t("nav.send"), t("send.subtitle")),
            Page::Receive => (t("nav.receive"), t("receive.subtitle")),
            Page::History => (t("nav.history"), t("history.subtitle")),
            Page::Logs => (t("nav.logs"), t("logs.subtitle")),
            Page::About => (t("nav.about"), t("about.subtitle")),
            Page::Settings => (t("nav.settings"), t("settings.subtitle")),
        };
        self.gfx.text(title, Font::Title, Rect::new(x, TITLE_H + 24.0, width, 40.0), th.text, Align::Start);
        self.gfx.text(subtitle, Font::Body, Rect::new(x, TITLE_H + 66.0, width, 22.0), th.text_secondary, Align::Start);
        if self.page == Page::Logs {
            self.draw_logs(x, width, h);
            return;
        }
        let view = Rect::new(NAV_W + 1.0, VIEW_TOP - 8.0, w - NAV_W - 1.0, h - VIEW_TOP + 8.0);
        self.gfx.push_clip(view);
        self.clip = Some(view);
        let k = 1.0 - (-self.dt * 14.0).exp();
        self.scroll += (self.scroll_target - self.scroll) * k;
        if (self.scroll_target - self.scroll).abs() < 0.5 {
            self.scroll = self.scroll_target;
        } else {
            self.animating = true;
        }
        let top = VIEW_TOP - self.scroll;
        let mut y = self.draw_info(x, top, width);
        y = match self.page {
            Page::Send => self.draw_send(x, y, width),
            Page::Receive => {
                let y = self.draw_receive(x, y, width);
                if std::mem::take(&mut self.inert_layer) {
                    self.gfx.pop_layer();
                    self.inert = false;
                }
                y
            }
            Page::Settings => self.draw_settings(x, y, width),
            Page::History => self.draw_history(x, y, width),
            _ => self.draw_about(x, y, width),
        };
        let content = y - top + 32.0;
        self.scroll_max = (content - view.h + 8.0).max(0.0);
        self.scroll = self.scroll.min(self.scroll_max);
        self.scroll_target = self.scroll_target.min(self.scroll_max);
        self.clip = None;
        self.gfx.pop_clip();
        // A hairline under the page title once the content scrolls beneath it.
        let lifted = self.anim((Id::PageFade, 1), if self.scroll > 1.0 { 1.0 } else { 0.0 }, 12.0);
        if lifted > 0.01 {
            self.gfx.line(view.x, view.y, view.right(), view.y, 1.0, th.divider_subtle().alpha(lifted * if th.high_contrast { 1.0 } else { 1.6 }));
        }
        if self.scroll_max > 0.0 {
            let track = view.h - 16.0;
            let thumb = (view.h / content * track).max(32.0);
            let pos = view.y + 8.0 + (track - thumb) * (self.scroll / self.scroll_max);
            self.gfx.fill(Rect::new(w - 7.0, pos, 3.0, thumb), 1.5, th.control_strong.alpha(0.8));
        }
    }

    fn forced_link(&self) -> Option<Link> {
        Some(match self.preview_link? {
            0 => Link::Off,
            1 => Link::Idle,
            2 => Link::Waiting,
            3 => Link::Connecting,
            _ => Link::Flowing,
        })
    }

    fn draw_info(&mut self, x: f32, y: f32, w: f32) -> f32 {
        let Some(info) = &self.info else { return y };
        let th = self.theme;
        let title = info.title.clone();
        let message = info.message.clone();
        let text_w = w - 48.0 - 56.0;
        let (title_w, _) = self.gfx.measure(&title, Font::BodyStrong, text_w, false);
        let (_, msg_h) = self.gfx.measure(&message, Font::Body, text_w - title_w - 12.0, true);
        let height = (msg_h + 28.0).max(48.0);
        let rect = Rect::new(x, y, w, height);
        self.gfx.fill(rect, 4.0, th.critical_background);
        self.gfx.stroke(rect, 4.0, 1.0, th.card_stroke);
        self.gfx.circle(x + 24.0, y + 24.0, 7.0, if th.dark { Color::rgb(0, 0, 0) } else { Color::rgb(255, 255, 255) });
        self.gfx.glyph(glyph::ERROR, Font::Icon, x + 24.0, y + 24.0, th.critical);
        self.gfx.text(&title, Font::BodyStrong, Rect::new(x + 48.0, y + 14.0, title_w + 2.0, 20.0), th.text, Align::Start);
        self.gfx.paragraph(&message, Font::Body, Rect::new(x + 48.0 + title_w + 12.0, y + 14.0, text_w - title_w - 12.0, msg_h + 4.0), th.text);
        self.button(Id::InfoClose, Rect::new(rect.right() - 44.0, y + 8.0, 32.0, 32.0), "", Some(glyph::CLOSE), Style::Subtle, true);
        y + height + 16.0
    }

    // Large card at the top of a page: illustration on the left, state and main action on the right.
    // `art`: (this PC, other device, PC on the left, state, animation slot).
    // `action_h`: height of the controls the caller puts under the text (0 for none), so the
    // whole text block is centred vertically.
    fn hero(&mut self, rect: Rect, art: (char, char, bool, Link, u8), headline: &str, body: &str, action_h: f32) -> Rect {
        let th = self.theme;
        self.gfx.fill(rect, 8.0, th.card);
        let art_w = (rect.w * 0.44).clamp(260.0, 440.0);
        // Aurora: three slow drifting lights (accent, pink, amber) across the card. Faint at rest,
        // brighter while something is happening, gone when the feature is off.
        let target = match art.3 {
            Link::Off => 0.0,
            Link::Idle => 0.45,
            Link::Waiting => 0.75,
            Link::Connecting | Link::Flowing => 1.0,
        };
        let energy = self.anim((Id::LinkArt, 40 + art.4), target, 3.0);
        if energy > 0.01 && !th.high_contrast {
            let time = self.started.elapsed().as_secs_f32();
            let strength = if th.dark { 0.26 } else { 0.16 };
            let lights = [
                (th.accent, 0.18, 0.55, 1.0),
                (Color::rgb(0xFF, 0x4F, 0x8B), 0.52, 0.15, 0.7),
                (Color::rgb(0xFF, 0xB3, 0x47), 0.80, 0.95, 0.6),
            ];
            self.gfx.push_clip(rect.inset(1.0, 1.0));
            for (i, (color, bx, by, weight)) in lights.into_iter().enumerate() {
                let phase = i as f32 * 2.1;
                let cx = rect.x + rect.w * (bx + 0.07 * (time * 0.19 + phase).sin());
                let cy = rect.y + rect.h * (by + 0.18 * (time * 0.15 + phase * 1.3).cos());
                self.gfx.glow_ellipse(cx, cy, rect.w * 0.34, rect.h * 0.95, color.alpha(strength * weight * energy));
            }
            self.gfx.pop_clip();
            if energy < 0.999 && energy != target {
                self.animating = true;
            } else if target > 0.0 {
                self.ambient = true;
            }
        }
        self.gfx.stroke(rect, 8.0, 1.0, th.card_stroke);
        self.link_art(Rect::new(rect.x + 20.0, rect.y + 20.0, art_w - 40.0, rect.h - 40.0), art.0, art.1, art.2, art.3, art.4);
        let tx = rect.x + art_w + 12.0;
        let tw = rect.right() - tx - 28.0;
        let (_, bh) = if body.is_empty() { (0.0, 0.0) } else { self.gfx.measure(body, Font::Body, tw, true) };
        let block = 28.0 + if bh > 0.0 { 8.0 + bh } else { 0.0 } + if action_h > 0.0 { 20.0 + action_h } else { 0.0 };
        let top = (rect.y + (rect.h - block) / 2.0).max(rect.y + 20.0);
        self.gfx.text(headline, Font::Subtitle, Rect::new(tx, top, tw, 28.0), th.text, Align::Start);
        let body_y = top + 36.0;
        if bh > 0.0 {
            self.gfx.paragraph(body, Font::Body, Rect::new(tx, body_y, tw, bh + 4.0), th.text_secondary);
        }
        let action_y = if bh > 0.0 { body_y + bh + 20.0 } else { top + 48.0 };
        Rect::new(tx, action_y, tw, rect.bottom() - action_y)
    }

    fn draw_send(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        let idle = self.mirror == Mirror::Idle;
        let target = self.mirror.key().and_then(|k| self.device_name(k)).unwrap_or(t("common.the_tv")).to_owned();
        let (headline, body, link) = match &self.mirror {
            Mirror::Active { width, height, .. } => (
                tf("send.hero.mirroring", &[("name", &target)]),
                tf(
                    "send.hero.details",
                    &[
                        ("width", width),
                        ("height", height),
                        ("fps", &self.settings.fps),
                        ("audio", &t(if self.settings.audio { "send.hero.with_audio" } else { "send.hero.without_audio" })),
                    ],
                ),
                Link::Flowing,
            ),
            Mirror::Connecting { .. } => (
                tf("send.hero.connecting", &[("name", &target)]),
                t("send.hero.connecting_body").to_owned(),
                Link::Connecting,
            ),
            Mirror::Reconnecting { attempt, .. } => (
                tf("send.hero.reconnecting", &[("name", &target)]),
                tf("send.hero.reconnecting_body", &[("attempt", attempt)]),
                Link::Connecting,
            ),
            Mirror::Stopping { .. } => (t("common.stopping").to_owned(), t("send.hero.stopping_body").to_owned(), Link::Connecting),
            Mirror::Idle if self.devices.is_empty() && (self.discovering || !self.searched) => (
                t("send.hero.searching").to_owned(),
                t("send.hero.searching_body").to_owned(),
                Link::Waiting,
            ),
            Mirror::Idle => (
                t("send.hero.ready").to_owned(),
                t("send.hero.ready_body").to_owned(),
                Link::Idle,
            ),
        };
        let link = self.forced_link().unwrap_or(link);
        let hero = Rect::new(x, y, w, HERO_H);
        let has_action = matches!(self.mirror, Mirror::Active { .. } | Mirror::Connecting { .. } | Mirror::Reconnecting { .. });
        let action = self.hero(hero, (glyph::PC, glyph::TV, true, link, 0), &headline, &body, if has_action { 32.0 } else { 0.0 });
        match &self.mirror {
            Mirror::Active { .. } => {
                let bw = self.button_width(t("common.stop"), Some(glyph::STOP));
                self.button(Id::HeroStop, Rect::new(action.x, action.y, bw, 32.0), t("common.stop"), Some(glyph::STOP), Style::Accent, true);
                self.badge(action.x + bw + 12.0, action.y + 16.0, t("common.live"), th.success);
            }
            Mirror::Connecting { .. } | Mirror::Reconnecting { .. } => {
                let bw = self.button_width(t("common.cancel"), None);
                self.button(Id::HeroStop, Rect::new(action.x, action.y, bw, 32.0), t("common.cancel"), None, Style::Standard, true);
                self.spinner(action.x + bw + 24.0, action.y + 16.0, 8.0, th.accent);
            }
            _ => {}
        }
        y += HERO_H + 32.0;

        self.section(x, y + 6.0, w, t("send.devices.title"));
        let refresh_w = self.button_width(t("send.devices.refresh"), Some(glyph::REFRESH));
        self.button(Id::Refresh, Rect::new(x + w - refresh_w, y, refresh_w, 32.0), t("send.devices.refresh"), Some(glyph::REFRESH), Style::Subtle, !self.discovering);
        if self.discovering {
            self.spinner(x + w - refresh_w - 18.0, y + 16.0, 8.0, th.accent);
        }
        y += 42.0;

        if self.devices.is_empty() && self.settings.favorites.is_empty() {
            // Same height as a device row, so the list does not jump when the first one appears.
            let rect = Rect::new(x, y, w, 72.0);
            self.card(rect);
            let cy = rect.y + rect.h / 2.0;
            let tile = Rect::new(rect.x + 16.0, cy - 20.0, 40.0, 40.0);
            self.gfx.fill(tile, 8.0, th.text_secondary.alpha(0.1));
            let tx = rect.x + 72.0;
            let tw = rect.w - 72.0 - 16.0;
            if self.discovering || !self.searched {
                self.spinner(tile.x + 20.0, tile.y + 20.0, 9.0, th.accent);
                self.gfx.text(t("send.devices.searching"), Font::Body, Rect::new(tx, rect.y, tw, rect.h), th.text_secondary, Align::Start);
            } else {
                self.gfx.glyph(glyph::WIFI, Font::Icon20, tile.x + 20.0, tile.y + 20.0, th.text_secondary);
                self.gfx.text(t("send.devices.none"), Font::Body, Rect::new(tx, cy - 20.0, tw, 20.0), th.text, Align::Start);
                self.gfx.text(t("send.devices.none_hint"), Font::Caption, Rect::new(tx, cy, tw, 18.0), th.text_secondary, Align::Start);
            }
            y += rect.h;
        } else {
            y = self.draw_device_cards(x, y, w);
        }
        y += 28.0;

        self.section(x, y, w, t("send.settings.title"));
        y += 30.0;
        let source = self.combo_label(ComboKind::Source);
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::SCREEN, t("send.source"), t("send.source_desc"), 260.0, idle);
        self.combo_box(ComboKind::Source, Rect::new(rect.right() - 16.0 - 260.0, rect.y + 18.0, 260.0, 32.0), &source, idle);
        y += ROW_H + GAP;
        if self.settings.share_window {
            let window = self.combo_label(ComboKind::Window);
            let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::WINDOW, t("send.window"), t("send.window_desc"), 380.0, idle);
            self.combo_box(ComboKind::Window, Rect::new(rect.right() - 16.0 - 380.0, rect.y + 18.0, 380.0, 32.0), &window, idle);
        } else {
            let monitor = self.combo_label(ComboKind::Monitor);
            let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::PC, t("send.settings.display"), t("send.settings.display_desc"), 260.0, idle);
            self.combo_box(ComboKind::Monitor, Rect::new(rect.right() - 16.0 - 260.0, rect.y + 18.0, 260.0, 32.0), &monitor, idle);
        }
        y += ROW_H + GAP;

        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::SendQuality,
            header,
            glyph::QUALITY,
            t("send.settings.image"),
            t("send.settings.image_desc"),
            "",
            3,
            0.0,
        );
        self.clipped(body, |app| {
            let rows: [(&str, &str, ComboKind, f32); 3] = [
                (t("send.settings.quality"), t("send.settings.quality_desc"), ComboKind::Quality, 220.0),
                (t("send.settings.resolution"), t("send.settings.resolution_desc"), ComboKind::Resolution, 220.0),
                (t("send.settings.fps"), t("send.settings.fps_desc"), ComboKind::Fps, 220.0),
            ];
            for (i, (title, description, kind, cw)) in rows.into_iter().enumerate() {
                let r = app.child_row(body, i, title, description, idle);
                let label = app.combo_label(kind);
                app.combo_box(kind, Rect::new(r.right() - 16.0 - cw, r.y + 14.0, cw, 32.0), &label, idle);
            }
        });
        y += ROW_H + body.h + GAP;

        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::SendAudio,
            header,
            glyph::VOLUME,
            t("send.settings.sound"),
            t("send.settings.sound_desc"),
            "",
            2,
            0.0,
        );
        let audio = self.settings.audio;
        let audio_on = idle && audio;
        self.clipped(body, |app| {
            let r = app.child_row(body, 0, t("send.settings.audio"), t("send.settings.audio_desc"), idle);
            app.toggle_with_label(Id::AudioToggle, r.right() - 16.0, r.y + r.h / 2.0, audio, idle);
            let r = app.child_row(body, 1, t("send.settings.audio_delay"), t("send.settings.audio_delay_desc"), audio_on);
            let label = app.combo_label(ComboKind::AudioLatency);
            app.combo_box(ComboKind::AudioLatency, Rect::new(r.right() - 16.0 - 220.0, r.y + 14.0, 220.0, 32.0), &label, audio_on);
        });
        y += ROW_H + body.h + GAP;

        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::POINTER, t("send.settings.pointer"), t("send.settings.pointer_desc"), 160.0, idle);
        self.toggle_with_label(Id::CursorToggle, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.cursor, idle);
        y + ROW_H
    }

    fn draw_receive(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        let off = self.receiver_editable();
        let restart_pending = self.restart == super::Restart::Pending;
        let on = matches!(self.receiver, ReceiverState::On { .. } | ReceiverState::Starting);
        let name = self.settings.receiver_name.clone();
        let (headline, body, link) = match self.receiver {
            ReceiverState::On { .. } if !self.clients.is_empty() => {
                let client = &self.clients[self.clients.len() - 1];
                (
                    tf("receive.hero.connected", &[("name", &client.name)]),
                    client.status.clone().unwrap_or_else(|| t("receive.hero.waiting_share").to_owned()),
                    Link::Flowing,
                )
            }
            ReceiverState::On { .. } => (
                tf("status.visible", &[("name", &name)]),
                tf("receive.hero.visible_body", &[("name", &name)]),
                Link::Waiting,
            ),
            ReceiverState::Starting => (t("status.starting_receiver").to_owned(), t("receive.hero.starting_body").to_owned(), Link::Waiting),
            ReceiverState::Stopping => (t("receive.hero.stopping").to_owned(), String::new(), Link::Off),
            ReceiverState::Off => (
                t("receive.hero.off").to_owned(),
                t("receive.hero.off_body").to_owned(),
                Link::Off,
            ),
        };
        let (headline, body) = match (self.pin_hero(), self.receiver) {
            (Some(pin), ReceiverState::On { .. }) => pin,
            _ => (headline, body),
        };
        let link = self.forced_link().unwrap_or(link);
        let hero = Rect::new(x, y, w, HERO_H);
        let action = self.hero(hero, (glyph::PC, glyph::PHONE, false, link, 1), &headline, &body, 32.0);
        let busy = self.receiver == ReceiverState::Stopping;
        self.toggle(Id::RecvToggle, action.x, action.y + 16.0, on, !busy);
        let label = t(if on { "common.on" } else { "common.off" });
        self.gfx.text(label, Font::Body, Rect::new(action.x + 52.0, action.y + 6.0, 120.0, 20.0), th.text, Align::Start);
        y += HERO_H + 32.0;
        // With the receiver off every option below is dimmed and does not react.
        let inert = !matches!(self.receiver, ReceiverState::On { .. });
        let shown = self.anim((Id::PageFade, 2), if inert { 0.42 } else { 1.0 }, 12.0);
        self.gfx.push_layer(shown, 0.0);
        self.inert = inert;
        self.inert_layer = true;

        if !self.clients.is_empty() {
            self.section(x, y, w, t("receive.connected.title"));
            y += 30.0;
            for client in &self.clients {
                let rect = Rect::new(x, y, w, 60.0);
                self.card(rect);
                let cy = rect.y + rect.h / 2.0;
                let tile = Rect::new(rect.x + 16.0, cy - 18.0, 36.0, 36.0);
                self.gfx.fill(tile, 8.0, th.accent.alpha(0.16));
                self.gfx.glyph(glyph::PHONE, Font::Icon20, tile.x + 18.0, tile.y + 18.0, th.text);
                self.gfx.text(&client.name, Font::Body, Rect::new(rect.x + 66.0, rect.y, w / 2.0, rect.h), th.text, Align::Start);
                let status = client.status.as_deref().unwrap_or(t("receive.connected.default"));
                self.gfx.text(status, Font::Body, Rect::new(rect.x + w / 2.0, rect.y, w / 2.0 - 20.0, rect.h), th.text_secondary, Align::End);
                y += 60.0 + GAP;
            }
            y += 28.0;
        }

        y = self.draw_now_playing(x, y, w);
        if self.mirroring {
            y = self.draw_capture_controls(x, y, w);
        }

        self.section(x, y, w, t("receive.settings.title"));
        if restart_pending {
            self.gfx.text(
                t("receive.settings.pending"),
                Font::Caption,
                Rect::new(x, y, w, 20.0),
                th.text_tertiary,
                Align::End,
            );
        }
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::RENAME, t("receive.settings.name"), t("receive.settings.name_desc"), 260.0, off);
        self.text_box(Id::NameBox, Rect::new(rect.right() - 16.0 - 260.0, rect.y + 18.0, 260.0, 32.0), &name, off);
        y += ROW_H + GAP;

        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::RecvQuality,
            header,
            glyph::QUALITY,
            t("receive.settings.quality"),
            t("receive.settings.quality_desc"),
            "",
            4,
            0.0,
        );
        let quality_toggles = [
            (Id::RecvHqScaling, t("receive.settings.hq_scaling"), t("receive.settings.hq_scaling_desc"), self.settings.receiver_hq_scaling),
            (Id::RecvVsync, t("receive.settings.vsync"), t("receive.settings.vsync_desc"), self.settings.receiver_vsync),
        ];
        self.clipped(body, |app| {
            let r = app.child_row(body, 0, t("receive.settings.resolution"), t("receive.settings.resolution_desc"), off);
            let label = app.combo_label(ComboKind::RecvSize);
            app.combo_box(ComboKind::RecvSize, Rect::new(r.right() - 16.0 - 160.0, r.y + 14.0, 160.0, 32.0), &label, off);
            let r = app.child_row(body, 1, t("send.settings.fps"), t("receive.settings.fps_desc"), off);
            let label = app.combo_label(ComboKind::RecvFps);
            app.combo_box(ComboKind::RecvFps, Rect::new(r.right() - 16.0 - 160.0, r.y + 14.0, 160.0, 32.0), &label, off);
            for (i, (id, title, description, value)) in quality_toggles.into_iter().enumerate() {
                let r = app.child_row(body, i + 2, title, description, off);
                app.toggle_with_label(id, r.right() - 16.0, r.y + r.h / 2.0, value, off);
            }
        });
        y += ROW_H + body.h + GAP;

        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::RecvDisplay,
            header,
            glyph::SCREEN,
            t("receive.settings.display"),
            t("receive.settings.display_desc"),
            "",
            4,
            0.0,
        );
        let display_toggles = [
            (Id::RecvBlur, t("receive.settings.blur"), t("receive.settings.blur_desc"), self.settings.receiver_blur),
            (Id::RecvBackdrop, t("receive.settings.backdrop"), t("receive.settings.backdrop_desc"), self.settings.receiver_backdrop),
            (Id::RecvStats, t("receive.settings.stats"), t("receive.settings.stats_desc"), self.settings.receiver_stats),
        ];
        self.clipped(body, |app| {
            let r = app.child_row(body, 0, t("receive.settings.scaling"), t("receive.settings.scaling_desc"), off);
            let label = app.combo_label(ComboKind::RecvScaling);
            app.combo_box(ComboKind::RecvScaling, Rect::new(r.right() - 16.0 - 220.0, r.y + 14.0, 220.0, 32.0), &label, off);
            for (i, (id, title, description, value)) in display_toggles.into_iter().enumerate() {
                let r = app.child_row(body, i + 1, title, description, off);
                app.toggle_with_label(id, r.right() - 16.0, r.y + r.h / 2.0, value, off);
            }
        });
        y += ROW_H + body.h + GAP;

        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::RecvWindow,
            header,
            glyph::WINDOW,
            t("receive.settings.window"),
            t("receive.settings.window_desc"),
            "",
            8,
            0.0,
        );
        let toggles = [
            (Id::RecvFullscreen, t("receive.settings.fullscreen"), t("receive.settings.fullscreen_desc"), self.settings.receiver_fullscreen),
            (Id::RecvOnTop, t("receive.settings.on_top"), t("receive.settings.on_top_desc"), self.settings.receiver_on_top),
            (Id::RecvBorderless, t("receive.settings.borderless"), t("receive.settings.borderless_desc"), self.settings.receiver_borderless),
            (Id::RecvLock, t("receive.settings.lock"), t("receive.settings.lock_desc"), self.settings.receiver_lock),
            (Id::RecvControls, t("receive.settings.controls"), t("receive.settings.controls_desc"), self.settings.receiver_controls),
            (Id::RecvMulti, t("receive.settings.multi"), t("receive.settings.multi_desc"), self.settings.receiver_multi),
        ];
        self.clipped(body, |app| {
            for (i, (id, title, description, value)) in toggles.into_iter().enumerate() {
                let r = app.child_row(body, i, title, description, off);
                app.toggle_with_label(id, r.right() - 16.0, r.y + r.h / 2.0, value, off);
            }
            let r = app.child_row(body, 6, t("receive.settings.keep"), t("receive.settings.keep_desc"), off);
            let label = app.combo_label(ComboKind::RecvKeep);
            app.combo_box(ComboKind::RecvKeep, Rect::new(r.right() - 16.0 - 160.0, r.y + 14.0, 160.0, 32.0), &label, off);
            app.child_row(body, 7, t("receive.settings.shortcuts"), t("receive.settings.shortcuts_desc"), off);
        });
        y += ROW_H + body.h + GAP;

        y = self.draw_sound_settings(x, y, w, true);

        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::VOLUME, t("receive.settings.audio"), t("receive.settings.audio_desc"), 160.0, off);
        self.toggle_with_label(Id::RecvAudio, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.receiver_audio, off);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::VIDEO, t("receive.settings.app_videos"), t("receive.settings.app_videos_desc"), 160.0, off);
        self.toggle_with_label(Id::RecvAppVideos, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.receiver_app_videos, off);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::PHOTO, t("receive.settings.app_photos"), t("receive.settings.app_photos_desc"), 160.0, off);
        self.toggle_with_label(Id::RecvAppPhotos, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.receiver_app_photos, off);
        y += ROW_H + GAP;
        y = self.draw_access(x, y, w);
        y += 32.0;

        self.section(x, y, w, t("receive.capture.settings"));
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::VOLUME, t("receive.capture.audio"), t("receive.capture.audio_desc"), 160.0, true);
        self.toggle_with_label(Id::RecordAudio, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.record_audio, true);
        y += ROW_H + GAP;
        for (kind, title) in [
            (super::CaptureDir::Recordings, t("receive.capture.videos_folder")),
            (super::CaptureDir::Screenshots, t("receive.capture.shots_folder")),
        ] {
            let path = self.capture_dir(kind).display().to_string();
            let (change, open) = (t("receive.capture.change"), t("common.open"));
            let (change_w, open_w) = (self.button_width(change, None), self.button_width(open, None));
            let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::FOLDER, title, &path, change_w + open_w + 24.0, true);
            let by = rect.y + 18.0;
            let open_x = rect.right() - 16.0 - open_w;
            self.button(Id::OpenDir(kind), Rect::new(open_x, by, open_w, 32.0), open, None, Style::Standard, true);
            self.button(Id::PickDir(kind), Rect::new(open_x - 8.0 - change_w, by, change_w, 32.0), change, None, Style::Standard, true);
            y += ROW_H + GAP;
        }
        y - GAP
    }

    // Record and screenshot buttons while a screen is being mirrored, and the last saved file.
    fn draw_capture_controls(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        self.section(x, y, w, t("receive.capture.title"));
        y += 30.0;
        let elapsed = self.recording_since.map(|since| since.elapsed().as_secs());
        let description = match elapsed {
            Some(secs) => {
                let time = if secs >= 3600 {
                    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
                } else {
                    format!("{}:{:02}", secs / 60, secs % 60)
                };
                tf("receive.capture.recording_desc", &[("time", &time)])
            }
            None => t("receive.capture.idle_desc").to_owned(),
        };
        let shot = t("receive.capture.screenshot");
        let record = t(if elapsed.is_some() { "receive.capture.stop_record" } else { "receive.capture.record" });
        let (shot_w, record_w) = (self.button_width(shot, None), self.button_width(record, None));
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::SCREEN, t("receive.capture.item"), &description, shot_w + record_w + 24.0, true);
        let by = rect.y + 18.0;
        let shot_x = rect.right() - 16.0 - shot_w;
        self.button(Id::RecShot, Rect::new(shot_x, by, shot_w, 32.0), shot, None, Style::Standard, true);
        let record_x = shot_x - 8.0 - record_w;
        let style = if elapsed.is_some() { Style::Accent } else { Style::Standard };
        self.button(Id::RecRecord, Rect::new(record_x, by, record_w, 32.0), record, None, style, true);
        if elapsed.is_some() {
            self.gfx.circle(record_x - 16.0, rect.y + rect.h / 2.0, 5.0, th.critical);
        }
        y += ROW_H + GAP;
        if let Some(path) = self.last_saved.clone() {
            let open = t("receive.capture.open_folder");
            let open_w = self.button_width(open, None);
            let shown = path.display().to_string();
            let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::FOLDER, t("receive.capture.last"), &shown, open_w + 24.0, true);
            self.button(Id::RecReveal, Rect::new(rect.right() - 16.0 - open_w, rect.y + 18.0, open_w, 32.0), open, None, Style::Standard, true);
            y += ROW_H + GAP;
        }
        y + 28.0 - GAP
    }

    fn draw_settings(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        self.section(x, y, w, t("settings.appearance"));
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::THEME, t("settings.theme"), t("settings.theme_desc"), 280.0, true);
        let label = self.combo_label(ComboKind::Theme);
        self.combo_box(ComboKind::Theme, Rect::new(rect.right() - 16.0 - 280.0, rect.y + 18.0, 280.0, 32.0), &label, true);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::COLOR, t("settings.accent"), t("settings.accent_desc"), 280.0, true);
        let label = self.combo_label(ComboKind::Accent);
        self.combo_box(ComboKind::Accent, Rect::new(rect.right() - 16.0 - 280.0, rect.y + 18.0, 280.0, 32.0), &label, true);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::LANGUAGE, t("settings.language"), t("settings.language_desc"), 280.0, true);
        let label = self.combo_label(ComboKind::Language);
        self.combo_box(ComboKind::Language, Rect::new(rect.right() - 16.0 - 280.0, rect.y + 18.0, 280.0, 32.0), &label, true);
        y += ROW_H + 32.0;

        self.section(x, y, w, t("settings.startup"));
        y += 30.0;
        let rows = [
            (Id::Autostart, glyph::POWER, t("settings.autostart"), t("settings.autostart_desc"), self.autostart),
            (
                Id::CloseToTray,
                glyph::TRAY,
                t("settings.close_to_tray"),
                t("settings.close_to_tray_desc"),
                self.settings.close_to_tray,
            ),
        ];
        for (id, icon, title, description, value) in rows {
            // The portable build never touches the registry, so it cannot start with Windows.
            let portable_off = id == Id::Autostart && crate::autostart::is_portable();
            let description = if portable_off { t("settings.autostart_portable") } else { description };
            let rect = self.row(Rect::new(x, y, w, ROW_H), icon, title, description, 160.0, !portable_off);
            self.toggle_with_label(id, rect.right() - 16.0, rect.y + rect.h / 2.0, value, !portable_off);
            y += ROW_H + GAP;
        }
        y += 28.0;

        y = self.draw_extras_settings(x, y, w);
        self.section(x, y, w, t("settings.connection"));
        y += 30.0;
        let remember = self.settings.remember_device;
        let rows = [
            (Id::AutoReconnect, glyph::SYNC, t("settings.auto_reconnect"), t("settings.auto_reconnect_desc"), self.settings.auto_reconnect, true),
            (Id::RememberDevice, glyph::TV, t("settings.remember_device"), t("settings.remember_device_desc"), remember, true),
            (Id::ConnectOnStart, glyph::POWER, t("settings.connect_on_start"), t("settings.connect_on_start_desc"), self.settings.connect_on_start && remember, remember),
        ];
        for (id, icon, title, description, value, enabled) in rows {
            let rect = self.row(Rect::new(x, y, w, ROW_H), icon, title, description, 160.0, enabled);
            self.toggle_with_label(id, rect.right() - 16.0, rect.y + rect.h / 2.0, value, enabled);
            y += ROW_H + GAP;
        }
        y += 28.0;


        self.section(x, y, w, t("settings.network"));
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::SEARCH, t("settings.auto_discover"), t("settings.auto_discover_desc"), 160.0, true);
        self.toggle_with_label(Id::AutoDiscover, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.auto_discover, true);
        y += ROW_H + 32.0;

        self.section(x, y, w, t("settings.privacy"));
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::PRIVACY, t("settings.hide_ips"), t("settings.hide_ips_desc"), 160.0, true);
        self.toggle_with_label(Id::HideIps, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.hide_ips, true);
        y += ROW_H + GAP;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::HISTORY, t("settings.save_history"), t("settings.save_history_desc"), 160.0, true);
        self.toggle_with_label(Id::SaveHistory, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.save_history, true);
        y += ROW_H + 32.0;

        self.section(x, y, w, t("settings.developer"));
        y += 30.0;
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::LOG, t("settings.developer_logs"), t("settings.developer_logs_desc"), 160.0, true);
        self.toggle_with_label(Id::DeveloperLogs, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.developer_logs, true);
        y += ROW_H + 32.0;

        y = self.draw_setup_settings(x, y, w);
        self.section(x, y, w, t("settings.data"));
        y += 30.0;
        let folder = rustplay_core::identity::data_dir().map(|d| d.display().to_string()).unwrap_or_default();
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::FOLDER, t("settings.data_folder"), &folder, 140.0, true);
        let bw = self.button_width(t("common.open"), None);
        self.button(Id::OpenData, Rect::new(rect.right() - 16.0 - bw, rect.y + 18.0, bw, 32.0), t("common.open"), None, Style::Standard, true);
        y += ROW_H + GAP;
        let rect = self.row(
            Rect::new(x, y, w, ROW_H),
            glyph::RESET,
            t("settings.reset"),
            t("settings.reset_desc"),
            160.0,
            true,
        );
        let bw = self.button_width(t("settings.reset_button"), None);
        self.button(Id::ResetSettings, Rect::new(rect.right() - 16.0 - bw, rect.y + 18.0, bw, 32.0), t("settings.reset_button"), None, Style::Standard, true);
        y += ROW_H + 32.0;
        self.gfx.text(
            &format!("RustPlay {}  ·  GPL-3.0", env!("CARGO_PKG_VERSION")),
            Font::Caption,
            Rect::new(x, y, w, 20.0),
            th.text_tertiary,
            Align::Center,
        );
        y + 20.0
    }

    fn draw_about(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        y = self.draw_update_banner(x, y, w);
        let th = self.theme;
        let rect = Rect::new(x, y, w, 156.0);
        self.gfx.fill(rect, 8.0, th.card);
        self.gfx.push_clip(rect.inset(1.0, 1.0));
        self.gfx.glow(rect.x + 88.0, rect.y + rect.h / 2.0, 150.0, Color::rgb(0xEA, 0x5A, 0x1A).alpha(0.22));
        self.gfx.pop_clip();
        self.gfx.stroke(rect, 8.0, 1.0, th.card_stroke);
        if let Some(logo) = self.logo.clone() {
            self.gfx.image(&logo, Rect::new(rect.x + 32.0, rect.y + 22.0, 112.0, 112.0), 1.0);
        }
        let tx = rect.x + 172.0;
        let tw = w - 196.0;
        self.gfx.text("RustPlay", Font::Title, Rect::new(tx, rect.y + 26.0, tw, 40.0), th.text, Align::Start);
        self.badge(tx, rect.y + 82.0, &tf("about.version", &[("version", &env!("CARGO_PKG_VERSION"))]), th.accent);
        self.gfx.paragraph(t("about.tagline"), Font::Body, Rect::new(tx, rect.y + 104.0, tw, 44.0), th.text_secondary);
        y += rect.h + 32.0;

        self.section(x, y, w, t("about.why"));
        y += 30.0;
        let (_, why_h) = self.gfx.measure(t("about.why_body"), Font::Body, w - 40.0, true);
        let (_, quote_h) = self.gfx.measure(t("about.why_quote"), Font::Quote, w - 40.0, true);
        let card = Rect::new(x, y, w, why_h + quote_h + 52.0);
        self.card(card);
        self.gfx.paragraph(t("about.why_body"), Font::Body, Rect::new(x + 20.0, y + 18.0, w - 40.0, why_h + 4.0), th.text_secondary);
        self.gfx.paragraph(t("about.why_quote"), Font::Quote, Rect::new(x + 20.0, y + 34.0 + why_h, w - 40.0, quote_h + 4.0), th.text);
        y += card.h + 32.0;

        self.section(x, y, w, t("about.support"));
        y += 26.0;
        // The Microsoft Store package only links to the project page: no external donation links.
        let store = crate::autostart::is_packaged();
        let support_body = if store { t("about.support_body_store") } else { t("about.support_body") };
        let (_, body_h) = self.gfx.measure(support_body, Font::Body, w, true);
        self.gfx.paragraph(support_body, Font::Body, Rect::new(x + 2.0, y, w, body_h + 4.0), th.text_secondary);
        y += body_h + 14.0;
        let tiles = [
            (LinkKind::GitHub, "GitHub", t("about.github_desc"), Color::rgb(0x24, 0x29, 0x2F), Color::rgb(0xFF, 0xFF, 0xFF)),
            (LinkKind::Coffee, "Ko-fi", t("about.coffee_desc"), Color::rgb(0xFF, 0x5E, 0x5B), Color::rgb(0xFF, 0xFF, 0xFF)),
            (LinkKind::PayPal, "PayPal", t("about.paypal_desc"), Color::rgb(0x00, 0x30, 0x87), Color::rgb(0xFF, 0xFF, 0xFF)),
        ];
        let count = if store { 1 } else { tiles.len() };
        let columns = if count > 1 && (w - 16.0) / 3.0 >= 220.0 { 3 } else { 1 };
        let tile_w = if columns == 3 { (w - 16.0) / 3.0 } else { w };
        let tile_h = 76.0;
        for (i, (kind, title, description, background, foreground)) in tiles.into_iter().enumerate().take(count) {
            let (col, row) = (i % columns, i / columns);
            let rect = Rect::new(x + col as f32 * (tile_w + 8.0), y + row as f32 * (tile_h + GAP), tile_w, tile_h);
            let hover = self.card_interactive(Id::Link(kind), rect, true);
            let cy = rect.y + rect.h / 2.0;
            let badge = Rect::new(rect.x + 16.0, cy - 22.0, 44.0, 44.0);
            self.gfx.fill(badge, 10.0, background);
            self.gfx.stroke(badge, 10.0, 1.0, Color::argb(0x1F00_0000));
            if let Some(icon) = self.brand_icons.get(i).cloned() {
                self.gfx.fill_icon(&icon, badge.x + 10.0, badge.y + 10.0, 24.0, foreground);
            }
            let tx = badge.right() + 14.0;
            let tw = rect.right() - tx - 36.0;
            self.gfx.text(title, Font::BodyStrong, Rect::new(tx, cy - 20.0, tw, 20.0), th.text, Align::Start);
            self.gfx.text(description, Font::Caption, Rect::new(tx, cy + 1.0, tw, 18.0), th.text_secondary, Align::Start);
            let arrow = th.text_tertiary.mix(th.text, hover);
            self.gfx.glyph(glyph::OPEN_EXTERNAL, Font::IconSmall, rect.right() - 20.0, rect.y + 20.0, arrow);
        }
        let rows = tiles_rows(columns, count);
        y += rows as f32 * (tile_h + GAP) + 28.0;

        let blocks: [(&str, &str); 3] = [
            (t("about.license"), t("about.license_body")),
            (t("about.credits"), t("about.credits_body")),
            (t("about.trademarks"), t("about.trademarks_body")),
        ];
        for (title, body) in blocks {
            self.section(x, y, w, title);
            y += 30.0;
            let (_, th) = self.gfx.measure(body, Font::Body, w - 40.0, true);
            let rect = Rect::new(x, y, w, th + 32.0);
            self.card(rect);
            self.gfx.paragraph(body, Font::Body, Rect::new(rect.x + 20.0, rect.y + 16.0, w - 40.0, th + 4.0), self.theme.text_secondary);
            y += rect.h + 24.0;
        }
        y
    }

    pub(super) fn draw_combo_popup(&mut self, w: f32, h: f32) {
        let Some(kind) = self.combo else { return };
        let th = self.theme;
        self.region(Rect::new(0.0, 0.0, w, h), Id::ComboDismiss);
        let items = self.combo_items(kind);
        let selected = self.combo_selected(kind);
        let item_h = 36.0;
        let anchor = self.combo_anchor;
        // Long lists (many open windows) show what fits and scroll with the wheel.
        let fits = (((h - TITLE_H - 24.0) / item_h) as usize).max(3);
        let visible = items.len().min(fits);
        let first = self.combo_scroll.min(items.len() - visible);
        self.combo_scroll = first;
        self.combo_visible = visible;
        let hot = self.combo_hot;
        let height = visible as f32 * item_h + 8.0;
        let width = anchor.w.max(160.0);
        let slot = if (first..first + visible).contains(&selected) { selected - first } else { 0 };
        let top = (anchor.y - 4.0 - slot as f32 * item_h).clamp(TITLE_H, (h - height - 8.0).max(TITLE_H));
        let reveal = self.anim((Id::ComboDismiss, 0), 1.0, 22.0);
        let rect = Rect::new(anchor.x, top, width, height);
        let with_icons = kind == ComboKind::Window && !self.windows.is_empty();
        self.gfx.push_layer(reveal, -6.0 * (1.0 - reveal));
        self.shadow(rect, 8.0, 16.0);
        self.gfx.fill(rect, 8.0, th.flyout);
        self.gfx.stroke(rect, 8.0, 1.0, th.flyout_stroke);
        for (slot, i) in (first..first + visible).enumerate() {
            let label = &items[i];
            let item = Rect::new(rect.x + 4.0, rect.y + 4.0 + slot as f32 * item_h, rect.w - 8.0, item_h - 4.0);
            let id = Id::ComboItem(kind, i);
            let (hover, pressed) = self.interact(id, item, true);
            let hover = if hot == Some(i) { 1.0 } else { hover };
            self.hc_hover(item, 4.0, hover);
            let base = if i == selected { th.subtle_hover } else { Color::TRANSPARENT };
            let fill = if pressed { th.subtle_pressed } else { base.mix(th.subtle_hover.alpha(1.5), hover) };
            self.gfx.fill(item, 4.0, fill);
            if i == selected {
                self.gfx.fill(Rect::new(item.x, item.y + item.h / 2.0 - 8.0, 3.0, 16.0), 1.5, th.accent);
            }
            let mut text_x = item.x + 12.0;
            if with_icons {
                let icon = self.windows.get(i).and_then(|w| w.icon.clone());
                self.window_icon(icon.as_ref(), text_x, item.y + item.h / 2.0, true);
                text_x += 26.0;
            }
            self.gfx.text(label, Font::Body, Rect::new(text_x, item.y, item.right() - 8.0 - text_x, item.h), th.text, Align::Start);
        }
        self.gfx.pop_layer();
    }

    pub(super) fn draw_pin(&mut self, w: f32, h: f32) {
        let th = self.theme;
        let Some(dialog) = &self.pin else { return };
        let device = dialog.device.clone();
        let digits = dialog.digits.clone();
        let fade = self.anim((Id::Modal, 0), 1.0, 16.0);
        self.region(Rect::new(0.0, 0.0, w, h), Id::Modal);
        self.gfx.fill(Rect::new(0.0, 0.0, w, h), 0.0, th.smoke.alpha(fade));
        let dw = 440.0;
        let message = tf("pin.message", &[("digits", &PIN_LEN), ("name", &device)]);
        let (_, mh) = self.gfx.measure(&message, Font::Body, dw - 48.0, true);
        let body_h = 24.0 + 32.0 + 12.0 + mh + 24.0 + 64.0 + 28.0;
        let dh = body_h + 80.0;
        let rect = Rect::new((w - dw) / 2.0, (h - dh) / 2.0 + 16.0 * (1.0 - fade), dw, dh);
        self.gfx.push_layer(fade, 0.0);
        self.shadow(rect, 8.0, 32.0);
        self.gfx.fill(rect, 8.0, th.dialog);
        self.gfx.fill(Rect::new(rect.x, rect.bottom() - 80.0, dw, 80.0), 8.0, th.dialog_footer);
        self.gfx.fill(Rect::new(rect.x, rect.bottom() - 80.0, dw, 12.0), 0.0, th.dialog_footer);
        self.gfx.line(rect.x, rect.bottom() - 80.0, rect.right(), rect.bottom() - 80.0, 1.0, th.card_stroke);
        self.gfx.stroke(rect, 8.0, 1.0, th.flyout_stroke);
        let x = rect.x + 24.0;
        let mut y = rect.y + 24.0;
        self.gfx.text(t("pin.title"), Font::Subtitle, Rect::new(x, y, dw - 48.0, 28.0), th.text, Align::Start);
        y += 44.0;
        self.gfx.paragraph(&message, Font::Body, Rect::new(x, y, dw - 48.0, mh + 4.0), th.text);
        y += mh + 24.0;
        let box_w = 56.0;
        let gap = 12.0;
        let total = PIN_LEN as f32 * box_w + (PIN_LEN - 1) as f32 * gap;
        let bx = rect.x + (dw - total) / 2.0;
        self.region(Rect::new(bx, y, total, 64.0), Id::PinBox);
        let caret = self.caret_visible();
        for i in 0..PIN_LEN {
            let r = Rect::new(bx + i as f32 * (box_w + gap), y, box_w, 64.0);
            let active = i == digits.len();
            self.gfx.fill(r, 6.0, if active { th.input_active } else { th.control });
            self.gfx.stroke(r, 6.0, 1.0, th.control_stroke);
            self.gfx.push_clip(Rect::new(r.x + 1.0, r.y + 1.0, r.w - 2.0, r.h - 1.0));
            if active {
                self.gfx.fill(Rect::new(r.x, r.bottom() - 2.0, r.w, 2.0), 0.0, th.accent);
            } else {
                self.gfx.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), 0.0, th.control_strong);
            }
            self.gfx.pop_clip();
            if let Some(d) = digits.chars().nth(i) {
                let mut buf = [0u8; 4];
                self.gfx.text(d.encode_utf8(&mut buf), Font::Display, r, th.text, Align::Center);
            } else if active && caret {
                self.gfx.line(r.x + r.w / 2.0, r.y + 18.0, r.x + r.w / 2.0, r.bottom() - 18.0, 1.5, th.text);
            }
        }
        let fy = rect.bottom() - 80.0 + 24.0;
        let bw = (dw - 48.0 - 8.0) / 2.0;
        let ready = digits.len() == PIN_LEN;
        self.button(Id::PinConnect, Rect::new(x, fy, bw, 32.0), t("pin.pair"), None, Style::Accent, ready);
        self.button(Id::PinCancel, Rect::new(x + bw + 8.0, fy, bw, 32.0), t("common.cancel"), None, Style::Standard, true);
        self.gfx.pop_layer();
    }
}

fn tiles_rows(columns: usize, count: usize) -> usize {
    count.div_ceil(columns)
}

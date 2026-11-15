// Received audio: the "now playing" card with remote control buttons and the speaker /
// lip-sync options.
use std::sync::Arc;
use std::time::Instant;

use rustplay_core::media::audio::AudioDevice;
use rustplay_rx::{NowPlaying, RemoteCommand};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

use super::widgets::Style;
use super::{App, ComboKind, Exp, GAP, Id, ROW_H};
use crate::backend::Command;
use crate::gfx::{Align, Font, Rect};
use crate::i18n::{t, tf};
use crate::settings::{AUDIO_DELAY_MAX_MS, AUDIO_DELAY_STEP_MS, clamp_delay};

const CARD_H: f32 = 128.0;
const ART: f32 = 96.0;
const SLIDER_W: f32 = 220.0;

mod icon {
    pub const MUSIC: char = '\u{E8D6}';
    pub const PREVIOUS: char = '\u{E892}';
    pub const NEXT: char = '\u{E893}';
    pub const PLAY: char = '\u{E768}';
    pub const PAUSE: char = '\u{E769}';
    pub const SPEAKER: char = '\u{E7F5}';
}

pub(super) struct NowPlayingView {
    info: NowPlaying,
    // When `info.position_ms` was reported, to advance the progress bar while playing.
    at: Instant,
    artwork: Option<ID2D1Bitmap1>,
}

// Received audio state and controls owned by the App.
#[derive(Default)]
pub(super) struct AudioUi {
    now: Option<NowPlayingView>,
    devices: Vec<AudioDevice>,
    slider: Rect,
}

pub(super) fn format_time(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

pub(super) fn format_delay(ms: i32) -> String {
    if ms > 0 { format!("+{ms} ms") } else { format!("{ms} ms") }
}

fn same_artwork(a: &Option<Arc<[u8]>>, b: &Option<Arc<[u8]>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a[..] == b[..],
        (None, None) => true,
        _ => false,
    }
}

impl NowPlayingView {
    // Current (position, duration) in ms, moving on while the track plays.
    fn progress(&self) -> Option<(u32, u32)> {
        let duration = self.info.duration_ms?;
        let mut position = self.info.position_ms.unwrap_or(0);
        if self.info.playing {
            position = position.saturating_add(self.at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32);
        }
        Some((position.min(duration), duration))
    }

    fn subtitle(&self) -> String {
        [self.info.artist.as_deref(), self.info.album.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  ·  ")
    }
}

impl App {
    pub(super) fn on_now_playing(&mut self, info: NowPlaying) {
        if info.is_empty() && info.duration_ms.is_none() {
            self.audio_ui.now = None;
            return;
        }
        let previous = self.audio_ui.now.take();
        let artwork = match previous {
            Some(view) if same_artwork(&view.info.artwork, &info.artwork) => view.artwork,
            _ => info.artwork.as_deref().and_then(|bytes| match self.gfx.load_png(bytes) {
                Ok(bitmap) => Some(bitmap),
                Err(e) => {
                    tracing::debug!("no se pudo decodificar la carátula: {e}");
                    None
                }
            }),
        };
        self.audio_ui.now = Some(NowPlayingView { info, at: Instant::now(), artwork });
    }

    pub(super) fn clear_now_playing(&mut self) {
        self.audio_ui.now = None;
    }

    // True while the progress bar moves, so the page keeps redrawing slowly.
    pub(super) fn now_playing_ticks(&self) -> bool {
        self.audio_ui.now.as_ref().is_some_and(|n| n.info.playing && n.info.duration_ms.is_some())
    }

    pub(super) fn remote(&mut self, command: RemoteCommand) {
        if let Some(now) = self.audio_ui.now.as_mut()
            && command == RemoteCommand::PlayPause
        {
            // Shows the new state at once; the device confirms it with FLUSH or new progress.
            if let Some((position, _)) = now.progress() {
                now.info.position_ms = Some(position);
            }
            now.info.playing = !now.info.playing;
            now.at = Instant::now();
        }
        self.send(Command::Remote(command));
    }

    pub(super) fn audio_devices_refresh(&mut self) {
        match rustplay_core::media::audio::render_devices() {
            Ok(devices) => self.audio_ui.devices = devices,
            Err(e) => tracing::warn!("no se pudieron listar los altavoces: {e}"),
        }
    }

    pub(super) fn output_items(&self) -> Vec<String> {
        let mut items = vec![t("receive.settings.output.default").to_owned()];
        items.extend(self.audio_ui.devices.iter().map(|d| d.name.clone()));
        items
    }

    pub(super) fn output_selected(&self) -> usize {
        let id = &self.settings.receiver_output;
        if id.is_empty() {
            return 0;
        }
        self.audio_ui.devices.iter().position(|d| d.id == *id).map_or(0, |i| i + 1)
    }

    pub(super) fn select_output(&mut self, index: usize) {
        self.settings.receiver_output = index
            .checked_sub(1)
            .and_then(|i| self.audio_ui.devices.get(i))
            .map(|d| d.id.clone())
            .unwrap_or_default();
    }

    // ------------------------------------------------------------- delay slider

    fn delay_fraction(&self) -> f32 {
        (self.settings.receiver_audio_delay + AUDIO_DELAY_MAX_MS) as f32 / (2 * AUDIO_DELAY_MAX_MS) as f32
    }

    // Moves the delay slider to the pointer; returns true when the value changed.
    pub(super) fn drag_delay(&mut self, x: f32) -> bool {
        let track = self.audio_ui.slider;
        if track.w <= 0.0 {
            return false;
        }
        let f = ((x - track.x) / track.w).clamp(0.0, 1.0);
        let ms = (f * (2 * AUDIO_DELAY_MAX_MS) as f32) as i64 - i64::from(AUDIO_DELAY_MAX_MS);
        let ms = clamp_delay(ms);
        let changed = ms != self.settings.receiver_audio_delay;
        self.settings.receiver_audio_delay = ms;
        changed
    }

    pub(super) fn nudge_delay(&mut self, steps: i32) {
        let ms = self.settings.receiver_audio_delay + steps * AUDIO_DELAY_STEP_MS;
        self.settings.receiver_audio_delay = clamp_delay(i64::from(ms));
        self.receiver_changed();
    }

    // WinUI-style slider: track, filled part from the centre (zero) and a round thumb.
    fn slider(&mut self, id: Id, rect: Rect, fraction: f32, enabled: bool) {
        let (hover, pressed) = self.interact(id, rect.inset(-6.0, 0.0), enabled);
        let dragging = enabled && self.pressed == Some(id);
        let th = self.theme;
        let cy = rect.y + rect.h / 2.0;
        self.audio_ui.slider = Rect::new(rect.x, rect.y, rect.w, rect.h);
        let track = if enabled { th.control_strong } else { th.text_disabled };
        self.gfx.fill(Rect::new(rect.x, cy - 2.0, rect.w, 4.0), 2.0, track.alpha(0.6));
        let mid = rect.x + rect.w / 2.0;
        let pos = rect.x + rect.w * fraction.clamp(0.0, 1.0);
        let (a, b) = if pos < mid { (pos, mid) } else { (mid, pos) };
        let fill = if enabled { th.accent } else { th.text_disabled };
        self.gfx.fill(Rect::new(a, cy - 2.0, b - a, 4.0), 2.0, fill);
        self.gfx.fill(Rect::new(mid - 0.5, cy - 6.0, 1.0, 12.0), 0.0, track);
        self.gfx.circle(pos, cy, 10.0, th.control_stroke);
        self.gfx.circle(pos, cy, 9.5, if th.dark { th.control_strong.mix(th.card, 0.6) } else { th.card });
        let inner = if dragging || pressed { 4.5 } else { 5.0 + 1.5 * hover };
        self.gfx.circle(pos, cy, inner, fill);
    }

    // ------------------------------------------------------------- drawing

    pub(super) fn draw_now_playing(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let Some(now) = self.audio_ui.now.as_ref() else { return y };
        let th = self.theme;
        let title = now.info.title.clone().unwrap_or_else(|| t("receive.now_playing.unknown").to_owned());
        let subtitle = now.subtitle();
        let progress = now.progress();
        let playing = now.info.playing;
        let remote = now.info.remote;
        let artwork = now.artwork.clone();

        self.section(x, y, w, t("receive.now_playing.title"));
        y += 30.0;
        let rect = Rect::new(x, y, w, CARD_H);
        self.card(rect);
        let art = Rect::new(rect.x + 16.0, rect.y + 16.0, ART, ART);
        match &artwork {
            Some(bitmap) => self.gfx.image(bitmap, art, 1.0),
            None => {
                self.gfx.fill(art, 6.0, th.accent.alpha(0.16));
                self.gfx.glyph(icon::MUSIC, Font::IconLarge, art.x + ART / 2.0, art.y + ART / 2.0, th.text);
            }
        }
        let buttons_w = 3.0 * 40.0 + 2.0 * 4.0;
        let tx = art.right() + 20.0;
        let tw = (rect.right() - 20.0 - buttons_w - tx).max(80.0);
        self.gfx.text(&title, Font::Subtitle, Rect::new(tx, rect.y + 20.0, tw, 28.0), th.text, Align::Start);
        self.gfx.text(&subtitle, Font::Body, Rect::new(tx, rect.y + 50.0, tw, 20.0), th.text_secondary, Align::Start);
        if let Some((position, duration)) = progress {
            let by = rect.y + 88.0;
            let bar = Rect::new(tx, by, tw, 4.0);
            self.gfx.fill(bar, 2.0, th.control_strong.alpha(0.5));
            let f = if duration > 0 { position as f32 / duration as f32 } else { 0.0 };
            self.gfx.fill(Rect::new(bar.x, bar.y, bar.w * f, bar.h), 2.0, th.accent);
            let times = Rect::new(tx, by + 8.0, tw, 18.0);
            self.gfx.text(&format_time(position), Font::Caption, times, th.text_secondary, Align::Start);
            self.gfx.text(&format_time(duration), Font::Caption, times, th.text_secondary, Align::End);
        } else if !playing {
            self.gfx.text(t("receive.now_playing.paused"), Font::Caption, Rect::new(tx, rect.y + 84.0, tw, 18.0), th.text_secondary, Align::Start);
        }
        let cy = rect.y + rect.h / 2.0;
        let mut bx = rect.right() - 20.0 - buttons_w;
        let controls = [
            (Id::RemotePrevious, icon::PREVIOUS),
            (Id::RemotePlayPause, if playing { icon::PAUSE } else { icon::PLAY }),
            (Id::RemoteNext, icon::NEXT),
        ];
        for (id, glyph) in controls {
            self.button(id, Rect::new(bx, cy - 20.0, 40.0, 40.0), "", Some(glyph), Style::Subtle, remote);
            bx += 44.0;
        }
        if !remote {
            self.gfx.text(
                t("receive.now_playing.no_remote"),
                Font::Caption,
                Rect::new(rect.right() - 20.0 - buttons_w, cy + 22.0, buttons_w, 18.0),
                th.text_tertiary,
                Align::Center,
            );
        }
        y + CARD_H + 32.0
    }

    // "Sonido" expander: speakers and lip-sync delay.
    pub(super) fn draw_sound_settings(&mut self, x: f32, y: f32, w: f32, enabled: bool) -> f32 {
        let header = Rect::new(x, y, w, ROW_H);
        let summary = format_delay(self.settings.receiver_audio_delay);
        let (body, _) = self.expander(
            Exp::RecvSound,
            header,
            icon::SPEAKER,
            t("receive.settings.sound"),
            t("receive.settings.sound_desc"),
            if self.settings.receiver_audio_delay != 0 { &summary } else { "" },
            2,
            0.0,
        );
        let fraction = self.delay_fraction();
        let audio = enabled && self.settings.receiver_audio;
        self.clipped(body, |app| {
            let r = app.child_row(body, 0, t("receive.settings.output"), t("receive.settings.output_desc"), audio);
            let label = app.combo_label(ComboKind::RecvOutput);
            app.combo_box(ComboKind::RecvOutput, Rect::new(r.right() - 16.0 - 260.0, r.y + 14.0, 260.0, 32.0), &label, audio);
            let r = app.child_row(body, 1, t("receive.settings.delay"), t("receive.settings.delay_desc"), audio);
            let value_w = 64.0;
            let slider = Rect::new(r.right() - 16.0 - value_w - SLIDER_W, r.y + r.h / 2.0 - 12.0, SLIDER_W, 24.0);
            app.slider(Id::RecvDelay, slider, fraction, audio);
            let th = app.theme;
            let color = if audio { th.text } else { th.text_disabled };
            let value = tf("receive.settings.delay_value", &[("value", &format_delay(app.settings.receiver_audio_delay))]);
            app.gfx.text(&value, Font::Body, Rect::new(r.right() - 16.0 - value_w, r.y, value_w, r.h), color, Align::End);
        });
        y + ROW_H + body.h + GAP
    }

    // Click handling for the ids this module draws; returns false for other ids.
    pub(super) fn audio_click(&mut self, id: Id) -> bool {
        match id {
            Id::RemotePrevious => self.remote(RemoteCommand::Previous),
            Id::RemotePlayPause => self.remote(RemoteCommand::PlayPause),
            Id::RemoteNext => self.remote(RemoteCommand::Next),
            Id::RecvDelay => self.receiver_changed(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_delays_are_formatted() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(65_999), "1:05");
        assert_eq!(format_time(3_600_000), "60:00");
        assert_eq!(format_delay(120), "+120 ms");
        assert_eq!(format_delay(-40), "-40 ms");
        assert_eq!(format_delay(0), "0 ms");
    }

    #[test]
    fn delay_is_clamped_to_steps() {
        assert_eq!(clamp_delay(0), 0);
        assert_eq!(clamp_delay(14), 10);
        assert_eq!(clamp_delay(15), 20);
        assert_eq!(clamp_delay(-15), -20);
        assert_eq!(clamp_delay(9_999), 500);
        assert_eq!(clamp_delay(-9_999), -500);
    }
}

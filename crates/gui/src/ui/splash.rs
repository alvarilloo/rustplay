// Start-up animation: the logo pops in at the centre with a glow and two accent ripples, the
// name fades in under it, then the logo flies to its place in the navigation pane while the cover
// fades and the rest of the window comes in (see `App::intro`).

use super::{App, TITLE_H};
use crate::gfx::{Align, Font, Rect};

// Seconds the cover stays fully opaque, then how long the flight to the navigation pane takes.
pub(super) const HOLD: f32 = 0.7;
pub(super) const FLIGHT: f32 = 0.5;
pub(super) const END: f32 = HOLD + FLIGHT;
// Where the window content starts its own entrance, as the cover begins to clear.
pub(super) const SHELL_DELAY: f32 = HOLD + 0.1;

// Navigation pane logo: centre and size (see `draw_nav`).
pub(super) const NAV_LOGO: (f32, f32, f32) = (12.0 + 36.0, TITLE_H + 4.0 + 38.0, 56.0);

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn ease_out_back(t: f32) -> f32 {
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

impl App {
    // Seconds since the window first appeared while the start-up animation runs.
    pub(super) fn splash_time(&self) -> Option<f32> {
        let t = self.intro?.elapsed().as_secs_f32();
        (t < END).then_some(t)
    }

    // A click or a key press during the animation jumps to the finished window.
    pub(super) fn skip_splash(&mut self) -> bool {
        if self.splash_time().is_none() {
            return false;
        }
        let skip = std::time::Duration::from_secs_f32(END + 2.0);
        self.intro = std::time::Instant::now().checked_sub(skip).or(self.intro);
        true
    }

    pub(super) fn draw_splash(&mut self, w: f32, h: f32) {
        let Some(t) = self.splash_time() else { return };
        self.animating = true;
        let th = self.theme;
        let fly = ease_in_out(clamp01((t - HOLD) / FLIGHT));
        let cover = 1.0 - ease_out(clamp01((t - HOLD) / (FLIGHT * 0.8)));
        self.gfx.fill(Rect::new(0.0, 0.0, w, h), 0.0, th.solid_background.alpha(cover));

        let (cx0, cy0) = (w / 2.0, h / 2.0 - 24.0);
        let (tx, ty, ts) = NAV_LOGO;
        let pop = ease_out_back(clamp01(t / 0.55));
        let start_size = 120.0 * (0.55 + 0.45 * pop);
        let size = start_size + (ts - start_size) * fly;
        let cx = cx0 + (tx - cx0) * fly;
        let cy = cy0 + (ty - cy0) * fly;

        // Soft accent glow and two ripples behind the logo while it is in the centre.
        let centre = 1.0 - fly;
        self.gfx.glow(cx, cy, 190.0 * (0.6 + 0.4 * pop), th.accent.alpha(0.22 * centre * clamp01(t / 0.3)));
        for k in 0..2 {
            let p = (t - 0.18 - k as f32 * 0.2) / 0.75;
            if (0.0..1.0).contains(&p) {
                let r = 64.0 + 120.0 * ease_out(p);
                let alpha = 0.55 * (1.0 - p).powi(2) * centre;
                self.gfx.stroke(Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r), r, 2.0 * (1.0 - p) + 1.0, th.accent.alpha(alpha));
            }
        }

        if let Some(logo) = self.logo.clone() {
            let alpha = clamp01(t / 0.18);
            self.gfx.image(&logo, Rect::new(cx - size / 2.0, cy - size / 2.0, size, size), alpha);
        }

        // The name rises in under the logo and fades as the logo leaves.
        let word = ease_out(clamp01((t - 0.28) / 0.35)) * (1.0 - clamp01(fly * 2.5));
        if word > 0.0 {
            let y = cy0 + 120.0 / 2.0 + 22.0 + 10.0 * (1.0 - word);
            self.gfx.push_layer(word, 0.0);
            self.gfx.text("RustPlay", Font::Title, Rect::new(cx0 - 200.0, y, 400.0, 40.0), th.text, Align::Center);
            self.gfx.pop_layer();
        }
    }
}

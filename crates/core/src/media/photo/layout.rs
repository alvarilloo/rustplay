// Geometry and timing of the picture viewer: how a picture is fitted into the window, the
// Ken Burns zoom, the cross-fade curve and the small button pill (fullscreen, close).

const BUTTON: f32 = 40.0;
const GAP: f32 = 4.0;
const PAD: f32 = 6.0;
const MARGIN: f32 = 28.0;

// The controls hide this long after the pointer stops moving.
pub const HIDE_AFTER_MS: u64 = 2500;
pub const FADE_SECONDS: f32 = 0.15;
// Cross-fade between two pictures.
pub const DISSOLVE_SECONDS: f32 = 0.8;
// Ken Burns: the picture grows by this fraction over one slide.
pub const KEN_BURNS_ZOOM: f32 = 0.12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Fullscreen,
    Close,
}

pub const BUTTONS: [Button; 2] = [Button::Fullscreen, Button::Close];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

pub struct Layout {
    pub bar: Area,
    pub buttons: [Area; 2],
}

// Sizes are in pixels for `scale` = DPI / 96.
pub fn layout(width: f32, height: f32, scale: f32) -> Layout {
    let (b, g, p) = (BUTTON * scale, GAP * scale, PAD * scale);
    let w = 2.0 * b + g + 2.0 * p;
    let h = b + 2.0 * p;
    let bar = Area { x: ((width - w) / 2.0).round(), y: (height - MARGIN * scale - h).round(), w, h };
    let mut buttons = [Area::default(); 2];
    for (i, slot) in buttons.iter_mut().enumerate() {
        *slot = Area { x: bar.x + p + i as f32 * (b + g), y: bar.y + p, w: b, h: b };
    }
    Layout { bar, buttons }
}

impl Layout {
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.buttons.iter().position(|a| a.contains(x, y))
    }

    pub fn over_controls(&self, x: f32, y: f32) -> bool {
        self.bar.contains(x, y)
    }
}

// Rectangle (x, y, w, h) of a `pw` x `ph` picture fitted and centred in `ww` x `wh`, never
// stretched; the rest of the window stays black.
pub fn fit(pw: u32, ph: u32, ww: f32, wh: f32) -> Area {
    if pw == 0 || ph == 0 || ww <= 0.0 || wh <= 0.0 {
        return Area { x: 0.0, y: 0.0, w: ww.max(0.0), h: wh.max(0.0) };
    }
    let (w, h) = if ww * ph as f32 <= wh * pw as f32 {
        (ww, ww * ph as f32 / pw as f32)
    } else {
        (wh * pw as f32 / ph as f32, wh)
    };
    Area { x: (ww - w) / 2.0, y: (wh - h) / 2.0, w, h }
}

// Smooth start and end for the cross-fade.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// Ken Burns frame: `base` zoomed in around a point that depends on `variant` (so consecutive
// pictures drift in different directions); `progress` is 0..=1 along the slide.
pub fn ken_burns(base: Area, progress: f32, variant: usize) -> Area {
    let zoom = 1.0 + KEN_BURNS_ZOOM * progress.clamp(0.0, 1.0);
    let (fx, fy) = match variant % 4 {
        0 => (0.35, 0.35),
        1 => (0.65, 0.4),
        2 => (0.4, 0.65),
        _ => (0.65, 0.65),
    };
    let (ax, ay) = (base.x + base.w * fx, base.y + base.h * fy);
    Area { x: ax - (ax - base.x) * zoom, y: ay - (ay - base.y) * zoom, w: base.w * zoom, h: base.h * zoom }
}

// Opacity of the new picture `elapsed` seconds after it appeared (1 without a transition).
pub fn dissolve_alpha(elapsed: f32, seconds: f32) -> f32 {
    if seconds <= 0.0 { 1.0 } else { ease(elapsed / seconds) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_with_letterbox() {
        let wide = fit(1920, 1080, 1000.0, 1000.0);
        assert_eq!((wide.x, wide.w), (0.0, 1000.0));
        assert!((wide.h - 562.5).abs() < 0.01 && (wide.y - 218.75).abs() < 0.01);
        let tall = fit(1080, 1920, 1000.0, 1000.0);
        assert_eq!((tall.y, tall.h), (0.0, 1000.0));
        // Small pictures are scaled up to fill, keeping the ratio.
        let small = fit(10, 10, 400.0, 200.0);
        assert_eq!((small.x, small.w, small.h), (100.0, 200.0, 200.0));
        let none = fit(0, 0, 640.0, 480.0);
        assert_eq!((none.w, none.h), (640.0, 480.0));
    }

    #[test]
    fn ken_burns_zooms_in_place() {
        let base = fit(1600, 900, 1600.0, 900.0);
        assert_eq!(ken_burns(base, 0.0, 0), base);
        let end = ken_burns(base, 1.0, 0);
        assert!((end.w - base.w * 1.12).abs() < 0.01);
        // It still covers the whole window.
        assert!(end.x <= 0.0 && end.y <= 0.0 && end.x + end.w >= 1600.0 && end.y + end.h >= 900.0);
        assert_ne!(ken_burns(base, 1.0, 0).x, ken_burns(base, 1.0, 1).x);
    }

    #[test]
    fn dissolve_curve() {
        assert_eq!(dissolve_alpha(0.0, 0.8), 0.0);
        assert_eq!(dissolve_alpha(0.8, 0.8), 1.0);
        assert_eq!(dissolve_alpha(5.0, 0.8), 1.0);
        assert!((dissolve_alpha(0.4, 0.8) - 0.5).abs() < 1e-6);
        assert_eq!(dissolve_alpha(0.0, 0.0), 1.0);
    }

    #[test]
    fn buttons_are_hit_tested() {
        let l = layout(1000.0, 800.0, 1.0);
        for (i, a) in l.buttons.iter().enumerate() {
            assert_eq!(l.hit(a.x + 1.0, a.y + 1.0), Some(i));
        }
        assert_eq!(l.hit(10.0, 10.0), None);
        assert!(l.over_controls(l.bar.x + 1.0, l.bar.y + 1.0));
        assert!(!l.over_controls(5.0, 5.0));
    }
}

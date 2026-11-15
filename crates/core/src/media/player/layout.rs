// Geometry of the video window's controls: a pill of buttons near the bottom edge with a seek
// bar above it, and the flyout menus (speed, quality, subtitles) that open above the pill. Shared
// by the window procedure (hit-testing) and the renderer (drawing).

const BUTTON: f32 = 40.0;
const SPEED_BUTTON: f32 = 54.0;
const GAP: f32 = 4.0;
const PAD: f32 = 6.0;
const MARGIN: f32 = 28.0;
const SLIDER: f32 = 92.0;
const SLIDER_TRACK: f32 = 4.0;
const SEEK_WIDTH: f32 = 720.0;
const SEEK_HIT: f32 = 26.0;
const SEEK_TRACK: f32 = 5.0;
const SEEK_GAP: f32 = 22.0;
const MENU_ITEM: f32 = 34.0;
const MENU_PAD: f32 = 6.0;
const MENU_MAX_ROWS: usize = 12;

// The controls hide this long after the pointer stops moving.
pub const HIDE_AFTER_MS: u64 = 2500;
pub const FADE_SECONDS: f32 = 0.15;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Volume,
    PlayPause,
    Subtitles,
    Quality,
    Speed,
    Fullscreen,
    Stop,
}

impl Button {
    // The menu this button opens, if any.
    pub fn menu(self) -> Option<Menu> {
        match self {
            Button::Speed => Some(Menu::Speed),
            Button::Quality => Some(Menu::Quality),
            Button::Subtitles => Some(Menu::Subtitles),
            _ => None,
        }
    }

    fn width(self) -> f32 {
        if self == Button::Speed { SPEED_BUTTON } else { BUTTON }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Menu {
    Speed,
    Quality,
    Subtitles,
}

impl Menu {
    pub fn code(self) -> u32 {
        match self {
            Menu::Speed => 1,
            Menu::Quality => 2,
            Menu::Subtitles => 3,
        }
    }

    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Menu::Speed),
            2 => Some(Menu::Quality),
            3 => Some(Menu::Subtitles),
            _ => None,
        }
    }

    pub fn button(self) -> Button {
        match self {
            Menu::Speed => Button::Speed,
            Menu::Quality => Button::Quality,
            Menu::Subtitles => Button::Subtitles,
        }
    }
}

// Which optional buttons the current media offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Features {
    pub quality: bool,
    pub subtitles: bool,
}

impl Features {
    pub fn bits(self) -> u32 {
        u32::from(self.quality) | (u32::from(self.subtitles) << 1)
    }

    pub fn from_bits(bits: u32) -> Self {
        Self { quality: bits & 1 != 0, subtitles: bits & 2 != 0 }
    }
}

const MAX_SLOTS: usize = 7;

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

#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub button: Button,
    pub area: Area,
}

pub struct Layout {
    pub bar: Area,
    slots: [Slot; MAX_SLOTS],
    count: usize,
    // Clickable strip of the volume slider and the thin track drawn inside it.
    pub volume: Area,
    pub volume_track: Area,
    // Clickable strip of the seek bar and the thin track drawn inside it.
    pub seek: Area,
    pub track: Area,
}

// Sizes are in pixels for `scale` = DPI / 96.
pub fn layout(width: f32, height: f32, scale: f32, features: Features) -> Layout {
    let (g, p) = (GAP * scale, PAD * scale);
    let mut order = [Button::Volume; MAX_SLOTS];
    let mut count = 0;
    for button in [Button::Volume, Button::PlayPause, Button::Subtitles, Button::Quality, Button::Speed, Button::Fullscreen, Button::Stop] {
        let shown = match button {
            Button::Subtitles => features.subtitles,
            Button::Quality => features.quality,
            _ => true,
        };
        if shown {
            order[count] = button;
            count += 1;
        }
    }
    let slider = SLIDER * scale;
    let inner: f32 = order[..count].iter().map(|b| b.width() * scale).sum::<f32>() + g * (count as f32 - 1.0) + slider + g;
    let w = inner + 2.0 * p;
    let h = BUTTON * scale + 2.0 * p;
    let bar = Area { x: ((width - w) / 2.0).round(), y: (height - MARGIN * scale - h).round(), w, h };
    let mut slots = [Slot { button: Button::Volume, area: Area::default() }; MAX_SLOTS];
    let mut x = bar.x + p;
    let mut volume = Area::default();
    for (i, &button) in order[..count].iter().enumerate() {
        let bw = button.width() * scale;
        slots[i] = Slot { button, area: Area { x, y: bar.y + p, w: bw, h: BUTTON * scale } };
        x += bw + g;
        if button == Button::Volume {
            volume = Area { x, y: bar.y + p, w: slider, h: BUTTON * scale };
            x += slider + g;
        }
    }
    let tt = SLIDER_TRACK * scale;
    let inset = 8.0 * scale;
    let volume_track = Area { x: volume.x + inset, y: (volume.y + (volume.h - tt) / 2.0).round(), w: volume.w - 2.0 * inset, h: tt };
    let sw = (width - 40.0 * scale).min(SEEK_WIDTH * scale).max(1.0);
    let sy = bar.y - SEEK_GAP * scale - SEEK_HIT * scale / 2.0;
    let seek = Area { x: ((width - sw) / 2.0).round(), y: sy.round(), w: sw, h: SEEK_HIT * scale };
    let th = SEEK_TRACK * scale;
    let track = Area { x: seek.x, y: (seek.y + (seek.h - th) / 2.0).round(), w: sw, h: th };
    Layout { bar, slots, count, volume, volume_track, seek, track }
}

impl Layout {
    pub fn slots(&self) -> &[Slot] {
        &self.slots[..self.count]
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.slots().iter().position(|s| s.area.contains(x, y))
    }

    pub fn slot_of(&self, button: Button) -> Option<&Slot> {
        self.slots().iter().find(|s| s.button == button)
    }

    pub fn over_controls(&self, x: f32, y: f32) -> bool {
        self.bar.contains(x, y) || self.seek.contains(x, y)
    }

    // Position along the seek bar for pointer x, 0..=1.
    pub fn seek_fraction(&self, x: f32) -> f32 {
        ((x - self.track.x) / self.track.w).clamp(0.0, 1.0)
    }

    // Volume (0..=1) for pointer x over the slider.
    pub fn volume_fraction(&self, x: f32) -> f32 {
        ((x - self.volume_track.x) / self.volume_track.w).clamp(0.0, 1.0)
    }
}

// A flyout list opened above the pill, anchored to the button that opened it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuLayout {
    pub panel: Area,
    // Index of the first row shown (the list scrolls when it is long) and how many rows are shown.
    pub first: usize,
    pub rows: usize,
    item_h: f32,
    pad: f32,
}

// `chars` is the length of the longest label, which sets the width.
pub fn menu(l: &Layout, anchor: &Area, count: usize, chars: usize, scroll: usize, scale: f32, window_w: f32) -> MenuLayout {
    let rows = count.min(MENU_MAX_ROWS);
    let item_h = MENU_ITEM * scale;
    let pad = MENU_PAD * scale;
    let w = ((chars as f32 * 7.5 + 72.0) * scale).clamp(150.0 * scale, 340.0 * scale);
    let h = rows as f32 * item_h + 2.0 * pad;
    let x = (anchor.x + anchor.w / 2.0 - w / 2.0).clamp(8.0 * scale, (window_w - w - 8.0 * scale).max(8.0 * scale)).round();
    let y = (l.seek.y - 8.0 * scale - h).max(8.0 * scale).round();
    let first = scroll.min(count.saturating_sub(rows));
    MenuLayout { panel: Area { x, y, w, h }, first, rows, item_h, pad }
}

impl MenuLayout {
    // The row `index` (absolute) as an area, if it is visible.
    pub fn item(&self, index: usize) -> Option<Area> {
        if index < self.first || index >= self.first + self.rows {
            return None;
        }
        Some(Area {
            x: self.panel.x + self.pad,
            y: self.panel.y + self.pad + (index - self.first) as f32 * self.item_h,
            w: self.panel.w - 2.0 * self.pad,
            h: self.item_h,
        })
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        (self.first..self.first + self.rows).find(|&i| self.item(i).is_some_and(|a| a.contains(x, y)))
    }
}

// "m:ss" or "h:mm:ss".
pub fn format_time(seconds: f64) -> String {
    let total = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

// "1x", "0.75x", "1.25x".
pub fn format_rate(rate: f64) -> String {
    let text = format!("{rate:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}\u{00D7}")
}

// Volume as a whole percentage.
pub fn percent(volume: f32) -> u32 {
    (volume.clamp(0.0, 1.0) * 100.0).round() as u32
}

// The volume after `steps` presses of 5 % (negative to lower it).
pub fn step_volume(volume: f32, steps: i32) -> f32 {
    let percent = percent(volume) as i32 + steps * 5;
    percent.clamp(0, 100) as f32 / 100.0
}

// Where a relative seek of `delta` seconds from `from` ends up, inside [`low`, `high`].
pub fn relative_seek(from: f64, delta: f64, low: f64, high: f64) -> f64 {
    (from + delta).clamp(low, high.max(low))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Features = Features { quality: true, subtitles: true };

    #[test]
    fn buttons_and_seek_bar_are_hit_tested() {
        let l = layout(1000.0, 800.0, 1.0, ALL);
        assert_eq!(l.bar.x, ((1000.0 - l.bar.w) / 2.0f32).round());
        assert!(l.bar.y + l.bar.h <= 800.0 - 28.0);
        for (i, s) in l.slots().iter().enumerate() {
            assert_eq!(l.hit(s.area.x + 1.0, s.area.y + 1.0), Some(i));
            assert!(l.bar.contains(s.area.x, s.area.y) && l.bar.contains(s.area.x + s.area.w - 1.0, s.area.y + s.area.h - 1.0));
        }
        assert_eq!(l.hit(10.0, 10.0), None);
        assert!(l.seek.y + l.seek.h <= l.bar.y);
        assert!(l.over_controls(l.seek.x + 5.0, l.seek.y + 5.0));
        assert!(!l.over_controls(5.0, 5.0));
    }

    #[test]
    fn optional_buttons_come_and_go() {
        let names = |f: Features| layout(1000.0, 800.0, 1.0, f).slots().iter().map(|s| s.button).collect::<Vec<_>>();
        assert_eq!(names(Features::default()), [Button::Volume, Button::PlayPause, Button::Speed, Button::Fullscreen, Button::Stop]);
        assert_eq!(
            names(ALL),
            [Button::Volume, Button::PlayPause, Button::Subtitles, Button::Quality, Button::Speed, Button::Fullscreen, Button::Stop]
        );
        // Buttons never overlap and the slider sits right after the volume button.
        let l = layout(1000.0, 800.0, 1.5, ALL);
        for pair in l.slots().windows(2) {
            assert!(pair[0].area.x + pair[0].area.w <= pair[1].area.x);
        }
        assert!(l.volume.x >= l.slots()[0].area.x + l.slots()[0].area.w);
        assert!(l.volume.x + l.volume.w <= l.slots()[1].area.x);
        assert_eq!(Features::from_bits(ALL.bits()), ALL);
        assert_eq!(Features::from_bits(0), Features::default());
    }

    #[test]
    fn the_pill_stays_inside_narrow_windows() {
        // The bar may be wider than a tiny window, but it stays centred.
        let l = layout(300.0, 300.0, 1.0, ALL);
        assert!(l.bar.w > 0.0);
        assert_eq!(l.bar.x, ((300.0 - l.bar.w) / 2.0f32).round());
    }

    #[test]
    fn seek_fraction_is_clamped() {
        let l = layout(1000.0, 800.0, 1.0, ALL);
        assert_eq!(l.seek_fraction(-50.0), 0.0);
        assert_eq!(l.seek_fraction(5000.0), 1.0);
        let mid = l.seek_fraction(l.track.x + l.track.w / 2.0);
        assert!((mid - 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_volume_slider_maps_the_pointer_to_a_level() {
        let l = layout(1000.0, 800.0, 1.0, ALL);
        assert_eq!(l.volume_fraction(l.volume_track.x - 30.0), 0.0);
        assert_eq!(l.volume_fraction(l.volume_track.x + l.volume_track.w + 30.0), 1.0);
        assert!((l.volume_fraction(l.volume_track.x + l.volume_track.w / 2.0) - 0.5).abs() < 1e-6);
        assert!(l.volume.contains(l.volume_track.x + 1.0, l.volume_track.y));
    }

    #[test]
    fn menus_open_above_the_bar_and_scroll() {
        let l = layout(1000.0, 800.0, 1.0, ALL);
        let anchor = l.slot_of(Button::Speed).unwrap().area;
        let m = menu(&l, &anchor, 6, 5, 0, 1.0, 1000.0);
        assert_eq!((m.first, m.rows), (0, 6));
        assert!(m.panel.y + m.panel.h <= l.seek.y, "above the seek bar");
        assert!(m.panel.x >= 0.0 && m.panel.x + m.panel.w <= 1000.0);
        for i in 0..6 {
            let a = m.item(i).unwrap();
            assert_eq!(m.hit(a.x + 2.0, a.y + 2.0), Some(i));
        }
        assert_eq!(m.hit(m.panel.x - 5.0, m.panel.y + 20.0), None);
        // Long lists show 12 rows and scroll, never past the end.
        let long = menu(&l, &anchor, 40, 20, 100, 1.0, 1000.0);
        assert_eq!((long.rows, long.first), (12, 28));
        assert!(long.item(27).is_none() && long.item(39).is_some());
        let top = menu(&l, &anchor, 40, 20, 3, 1.0, 1000.0);
        assert_eq!(top.hit(top.panel.x + 10.0, top.panel.y + 6.0 + 2.0), Some(3));
        // A menu from a button near the window edge is pulled inside.
        let edge = menu(&l, &Area { x: 990.0, y: 700.0, w: 40.0, h: 40.0 }, 3, 30, 0, 1.0, 1000.0);
        assert!(edge.panel.x + edge.panel.w <= 1000.0);
        assert_eq!(Menu::from_code(Menu::Quality.code()), Some(Menu::Quality));
        assert_eq!(Menu::from_code(0), None);
        assert_eq!(Button::Quality.menu().map(Menu::button), Some(Button::Quality));
        assert_eq!(Button::Stop.menu(), None);
    }

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(75.9), "1:15");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(f64::NAN), "0:00");
    }

    #[test]
    fn formats_rates_and_volumes() {
        assert_eq!(format_rate(1.0), "1\u{00D7}");
        assert_eq!(format_rate(0.75), "0.75\u{00D7}");
        assert_eq!(format_rate(1.5), "1.5\u{00D7}");
        assert_eq!(format_rate(2.0), "2\u{00D7}");
        assert_eq!(percent(0.554), 55);
        assert_eq!(step_volume(1.0, 1), 1.0);
        assert_eq!(step_volume(0.5, 1), 0.55);
        assert_eq!(step_volume(0.5, -2), 0.4);
        assert_eq!(step_volume(0.03, -1), 0.0);
        assert_eq!(step_volume(0.5, 3), 0.65);
    }

    #[test]
    fn relative_seeks_stay_inside_the_media() {
        assert_eq!(relative_seek(10.0, 5.0, 0.0, 100.0), 15.0);
        assert_eq!(relative_seek(2.0, -5.0, 0.0, 100.0), 0.0);
        assert_eq!(relative_seek(98.0, 5.0, 0.0, 100.0), 100.0);
        // A live window that starts late.
        assert_eq!(relative_seek(52.0, -5.0, 50.0, 80.0), 50.0);
    }
}

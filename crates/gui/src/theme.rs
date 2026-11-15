use windows::UI::Color as WinColor;
use windows::UI::ViewManagement::{UIColorType, UISettings};

use crate::gfx::Color;

// WinUI 3 theme resources (Fluent 2), so the custom-drawn controls match Windows 11.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub dark: bool,
    // Windows "Contrast themes": every colour comes from the system palette and nothing is
    // translucent, so hover and pressed states rely on outlines instead of tints.
    pub high_contrast: bool,
    pub solid_background: Color,
    pub layer: Color,
    pub layer_stroke: Color,
    pub text: Color,
    pub text_secondary: Color,
    pub text_tertiary: Color,
    pub text_disabled: Color,
    pub text_on_accent: Color,
    pub accent: Color,
    pub card: Color,
    pub card_stroke: Color,
    pub card_hover: Color,
    pub card_secondary: Color,
    pub caution: Color,
    pub close_hover: Color,
    pub control: Color,
    pub control_hover: Color,
    pub control_pressed: Color,
    pub control_disabled: Color,
    pub control_stroke: Color,
    pub control_stroke_bottom: Color,
    pub control_strong: Color,
    pub input_active: Color,
    pub subtle_hover: Color,
    pub subtle_pressed: Color,
    pub smoke: Color,
    pub dialog: Color,
    pub dialog_footer: Color,
    pub flyout: Color,
    pub flyout_stroke: Color,
    pub critical: Color,
    pub critical_background: Color,
    pub success: Color,
    // Keyboard focus rectangle (FocusStrokeColorOuter / Inner).
    pub focus_outer: Color,
    pub focus_inner: Color,
}

fn from_win(c: WinColor) -> Color {
    Color::rgba(c.R, c.G, c.B, c.A)
}

impl Theme {
    // `preference`: 0 follows Windows, 1 forces light, 2 forces dark. `brand` uses RustPlay's
    // orange as the accent instead of the Windows accent colour.
    pub fn resolve(preference: usize, brand: bool) -> Self {
        if let Some(theme) = Self::high_contrast() {
            return theme;
        }
        let settings = UISettings::new().ok();
        let get = |kind: UIColorType| settings.as_ref().and_then(|s| s.GetColorValue(kind).ok()).map(from_win);
        // The foreground colour is white when the user has chosen the dark app mode.
        let dark = match preference {
            1 => false,
            2 => true,
            _ => get(UIColorType::Foreground).is_none_or(|c| c.r > 0.5),
        };
        // The brand tones keep the WinUI contrast pairs: black text on the light orange in dark
        // mode, white text on the deep orange in light mode.
        let accent = match (brand, dark) {
            (true, true) => Color::rgb(0xFF, 0x8F, 0x5A),
            (true, false) => Color::rgb(0xC4, 0x3E, 0x0A),
            (false, true) => get(UIColorType::AccentLight2).unwrap_or(Color::rgb(0x99, 0xEB, 0xFF)),
            (false, false) => get(UIColorType::AccentDark1).unwrap_or(Color::rgb(0x00, 0x5F, 0xB8)),
        };
        if dark { Self::dark(accent) } else { Self::light(accent) }
    }

    fn dark(accent: Color) -> Self {
        Self {
            dark: true,
            high_contrast: false,
            solid_background: Color::rgb(0x20, 0x20, 0x20),
            layer: Color::argb(0x4C3A_3A3A),
            layer_stroke: Color::argb(0x1900_0000),
            text: Color::argb(0xFFFF_FFFF),
            text_secondary: Color::argb(0xC5FF_FFFF),
            text_tertiary: Color::argb(0x87FF_FFFF),
            text_disabled: Color::argb(0x5DFF_FFFF),
            text_on_accent: Color::argb(0xFF00_0000),
            accent,
            card: Color::argb(0x0DFF_FFFF),
            card_stroke: Color::argb(0x1900_0000),
            card_hover: Color::argb(0x15FF_FFFF),
            card_secondary: Color::argb(0x08FF_FFFF),
            caution: Color::rgb(0xFC, 0xE1, 0x00),
            close_hover: Color::rgb(0xC4, 0x2B, 0x1C),
            control: Color::argb(0x0FFF_FFFF),
            control_hover: Color::argb(0x15FF_FFFF),
            control_pressed: Color::argb(0x08FF_FFFF),
            control_disabled: Color::argb(0x0BFF_FFFF),
            control_stroke: Color::argb(0x12FF_FFFF),
            control_stroke_bottom: Color::argb(0x18FF_FFFF),
            control_strong: Color::argb(0x8BFF_FFFF),
            input_active: Color::argb(0xB31E_1E1E),
            subtle_hover: Color::argb(0x0FFF_FFFF),
            subtle_pressed: Color::argb(0x0AFF_FFFF),
            smoke: Color::argb(0x4D00_0000),
            dialog: Color::rgb(0x2B, 0x2B, 0x2B),
            dialog_footer: Color::rgb(0x20, 0x20, 0x20),
            flyout: Color::rgb(0x2C, 0x2C, 0x2C),
            flyout_stroke: Color::argb(0x3300_0000),
            critical: Color::rgb(0xFF, 0x99, 0xA4),
            critical_background: Color::rgb(0x44, 0x27, 0x26),
            success: Color::rgb(0x6C, 0xCB, 0x5F),
            focus_outer: Color::argb(0xFFFF_FFFF),
            focus_inner: Color::argb(0xB300_0000),
        }
    }

    fn light(accent: Color) -> Self {
        Self {
            dark: false,
            high_contrast: false,
            solid_background: Color::rgb(0xF3, 0xF3, 0xF3),
            layer: Color::argb(0x80FF_FFFF),
            layer_stroke: Color::argb(0x0F00_0000),
            text: Color::argb(0xE400_0000),
            text_secondary: Color::argb(0x9E00_0000),
            text_tertiary: Color::argb(0x7200_0000),
            text_disabled: Color::argb(0x5C00_0000),
            text_on_accent: Color::argb(0xFFFF_FFFF),
            accent,
            card: Color::argb(0xB3FF_FFFF),
            card_stroke: Color::argb(0x0F00_0000),
            card_hover: Color::argb(0xF2FB_FBFB),
            card_secondary: Color::argb(0x80F6_F6F6),
            caution: Color::rgb(0x9D, 0x5D, 0x00),
            close_hover: Color::rgb(0xC4, 0x2B, 0x1C),
            control: Color::argb(0xB3FF_FFFF),
            control_hover: Color::argb(0x80F9_F9F9),
            control_pressed: Color::argb(0x4DF9_F9F9),
            control_disabled: Color::argb(0x4DF9_F9F9),
            control_stroke: Color::argb(0x0F00_0000),
            control_stroke_bottom: Color::argb(0x2900_0000),
            control_strong: Color::argb(0x7200_0000),
            input_active: Color::argb(0xFFFF_FFFF),
            subtle_hover: Color::argb(0x0900_0000),
            subtle_pressed: Color::argb(0x0600_0000),
            smoke: Color::argb(0x4D00_0000),
            dialog: Color::rgb(0xFF, 0xFF, 0xFF),
            dialog_footer: Color::rgb(0xF3, 0xF3, 0xF3),
            flyout: Color::rgb(0xF9, 0xF9, 0xF9),
            flyout_stroke: Color::argb(0x1F00_0000),
            critical: Color::rgb(0xC4, 0x2B, 0x1C),
            critical_background: Color::rgb(0xFD, 0xE7, 0xE9),
            success: Color::rgb(0x0F, 0x7B, 0x0F),
            focus_outer: Color::argb(0xE400_0000),
            focus_inner: Color::argb(0xB3FF_FFFF),
        }
    }

    // The active Contrast theme, built from the system colours, or None when it is off.
    fn high_contrast() -> Option<Self> {
        use windows::Win32::Graphics::Gdi::{
            COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT,
            COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor, SYS_COLOR_INDEX,
        };
        use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        let mut info = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
        // SAFETY: `info` is a valid HIGHCONTRASTW with its size set, as the call requires.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                info.cbSize,
                Some(std::ptr::from_mut(&mut info).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if ok.is_err() || (info.dwFlags.0 & HCF_HIGHCONTRASTON.0) == 0 {
            return None;
        }
        let sys = |index: SYS_COLOR_INDEX| {
            // SAFETY: GetSysColor only reads a system setting.
            let c = unsafe { GetSysColor(index) };
            Color::rgb((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
        };
        let window = sys(COLOR_WINDOW);
        let text = sys(COLOR_WINDOWTEXT);
        let highlight = sys(COLOR_HIGHLIGHT);
        let highlight_text = sys(COLOR_HIGHLIGHTTEXT);
        let gray = sys(COLOR_GRAYTEXT);
        let button = sys(COLOR_BTNFACE);
        let button_text = sys(COLOR_BTNTEXT);
        let link = sys(COLOR_HOTLIGHT);
        Some(Self {
            dark: window.r + window.g + window.b < 1.5,
            high_contrast: true,
            solid_background: window,
            layer: window,
            layer_stroke: text,
            text,
            text_secondary: text,
            text_tertiary: text,
            text_disabled: gray,
            text_on_accent: highlight_text,
            accent: highlight,
            card: window,
            card_stroke: text,
            card_hover: window,
            card_secondary: window,
            caution: link,
            close_hover: highlight,
            control: button,
            control_hover: button,
            control_pressed: button,
            control_disabled: window,
            control_stroke: button_text,
            control_stroke_bottom: button_text,
            control_strong: text,
            input_active: window,
            subtle_hover: Color::TRANSPARENT,
            subtle_pressed: Color::TRANSPARENT,
            smoke: Color::TRANSPARENT,
            dialog: window,
            dialog_footer: window,
            flyout: window,
            flyout_stroke: text,
            critical: link,
            critical_background: window,
            success: link,
            focus_outer: text,
            focus_inner: window,
        })
    }

    pub fn divider_subtle(&self) -> Color {
        if self.high_contrast { self.text } else { self.text.alpha(0.08) }
    }
}

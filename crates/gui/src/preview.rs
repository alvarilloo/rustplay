// Developer preview: with RUSTPLAY_PREVIEW=<folder>, RustPlay opens off screen without the network,
// the tray or saving settings, walks every page in light and dark (and the start-up
// animation) and saves PNG captures in that folder, then exits. Used to review the design.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDC, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::ui::{App, Page};

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("RUSTPLAY_PREVIEW").map(PathBuf::from)
}

pub fn enable() {
    ACTIVE.store(true, Ordering::Relaxed);
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

struct Shot {
    name: String,
    page: Page,
    dark: bool,
    // Fraction of the page's scroll range, or the moment of the start-up animation to capture.
    scroll: f32,
    intro_ms: Option<u64>,
    link: Option<u8>,
    // Show the custom-name editor of the first favourite.
    alias: bool,
    // 0 none, 1 setup assistant with its results, 2 setup assistant while checking.
    setup: u8,
    update: u8,
}

pub struct Script {
    dir: PathBuf,
    shots: Vec<Shot>,
    index: usize,
    applied: Option<Instant>,
}

const SETTLE: Duration = Duration::from_millis(1400);

impl Script {
    pub fn new(dir: PathBuf) -> Self {
        let mut shots = Vec::new();
        for ms in [0u64, 150, 300, 450, 600, 800, 1100, 1600] {
            shots.push(Shot { name: format!("intro-{ms:04}"), page: Page::Receive, dark: true, scroll: 0.0, intro_ms: Some(ms), link: None, alias: false, setup: 0, update: 0 });
        }
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            for (page, name) in [(Page::Send, "send"), (Page::Receive, "receive")] {
                for (link, state) in [(2u8, "waiting"), (3, "connecting"), (4, "connected")] {
                    shots.push(Shot { name: format!("hero-{name}-{state}-{theme}"), page, dark, scroll: 0.0, intro_ms: None, link: Some(link), alias: false, setup: 0, update: 0 });
                }
            }
        }
        let pages = [(Page::Send, "send"), (Page::Receive, "receive"), (Page::History, "history"), (Page::Logs, "logs"), (Page::About, "about"), (Page::Settings, "settings")];
        for dark in [true, false] {
            for (page, name) in pages {
                for (i, scroll) in [0.0f32, 0.5, 1.0].into_iter().enumerate() {
                    let theme = if dark { "dark" } else { "light" };
                    shots.push(Shot { name: format!("{name}-{theme}-{i}"), page, dark, scroll, intro_ms: None, link: None, alias: false, setup: 0, update: 0 });
                }
            }
        }
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            shots.push(Shot { name: format!("send-alias-{theme}"), page: Page::Send, dark, scroll: 0.3, intro_ms: None, link: None, alias: true, setup: 0, update: 0 });
            for (setup, name) in [(2u8, "checking"), (1, "results")] {
                shots.push(Shot { name: format!("setup-{name}-{theme}"), page: Page::Receive, dark, scroll: 0.0, intro_ms: None, link: None, alias: false, setup, update: 0 });
            }
        }
        for dark in [true, false] {
            let theme = if dark { "dark" } else { "light" };
            for step in 0..6u8 {
                shots.push(Shot { name: format!("welcome-{step}-{theme}"), page: Page::Receive, dark, scroll: 0.0, intro_ms: None, link: None, alias: false, setup: 10 + step, update: 0 });
            }
        }
        for (dark, update, name) in [(true, 1u8, "available"), (true, 2, "downloading"), (true, 3, "ready"), (false, 1, "available"), (false, 2, "downloading")] {
            let theme = if dark { "dark" } else { "light" };
            shots.push(Shot { name: format!("update-{name}-{theme}"), page: Page::About, dark, scroll: 0.0, intro_ms: None, link: None, alias: false, setup: 0, update });
        }
        Self { dir, shots, index: 0, applied: None }
    }

    // Advances the script; returns false once every capture is saved.
    pub fn tick(&mut self, hwnd: HWND, app: &mut App) -> bool {
        let Some(shot) = self.shots.get(self.index) else { return false };
        let Some(applied) = self.applied else {
            app.preview_show(shot.page, shot.dark, shot.intro_ms.is_some());
            app.preview_link(shot.link);
            app.preview_alias(shot.alias);
            app.preview_update(shot.update);
            app.preview_welcome(shot.setup.checked_sub(10).map(usize::from));
            match shot.setup {
                0 | 10..=19 => app.preview_setup_close(),
                busy => app.preview_setup(busy == 2),
            }
            self.applied = Some(Instant::now());
            return true;
        };
        let wait = shot.intro_ms.map_or(SETTLE, Duration::from_millis);
        if applied.elapsed() < wait {
            return true;
        }
        app.preview_scroll(shot.scroll);
        app.render();
        let path = self.dir.join(format!("{}.png", shot.name));
        match capture(hwnd).and_then(|image| rustplay_core::media::screenshot::save_png(&path, &image).map_err(|e| e.to_string())) {
            Ok(()) => tracing::info!("vista previa guardada: {}", path.display()),
            Err(e) => tracing::warn!("no se pudo guardar la vista previa {}: {e}", path.display()),
        }
        self.index += 1;
        self.applied = None;
        self.index < self.shots.len()
    }
}

fn capture(hwnd: HWND) -> Result<rustplay_core::media::screenshot::Image, String> {
    // SAFETY: GDI objects are created, used and released here; the window belongs to this thread.
    unsafe {
        let mut client = Default::default();
        GetClientRect(hwnd, &mut client).map_err(|e| e.to_string())?;
        let (w, h) = (client.right, client.bottom);
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).map_err(|e| e.to_string())?;
        let old = SelectObject(dc, bitmap.into());
        // PW_CLIENTONLY | PW_RENDERFULLCONTENT, so the DirectComposition content is included.
        let ok = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(1 | 2)).as_bool();
        let len = (w * h * 4) as usize;
        let mut pixels = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !ok {
            return Err("PrintWindow falló".into());
        }
        for px in pixels.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(rustplay_core::media::screenshot::Image { width: w as u32, height: h as u32, pixels })
    }
}

pub const TIMER: usize = 3;
pub const TICK_MS: u32 = 50;

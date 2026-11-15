// Playback engine of the video player: the Windows MediaPlayer in frame-server mode. Plain URLs
// open as a MediaSource; HLS playlists (what YouTube sends) open through AdaptiveMediaSource,
// which the older Media Foundation media engine cannot play. Each new frame is copied by the GPU
// into a texture of ours (`copy_frame`), so the rest of the player stays the same.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Foundation::{Rect as WinRect, TimeSpan, TypedEventHandler, Uri};
use windows_future::{AsyncStatus, IAsyncOperation};
use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
use windows::Media::Core::MediaSource;
use windows::Media::Playback::{MediaPlaybackState, MediaPlayer, MediaPlayerFailedEventArgs};
use windows::Media::Streaming::Adaptive::{
    AdaptiveMediaSource, AdaptiveMediaSourceCreationResult, AdaptiveMediaSourceCreationStatus,
    AdaptiveMediaSourceDownloadFailedEventArgs,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11SurfaceFromDXGISurface;
use windows::core::{HSTRING, IInspectable, Interface};

use super::native::{Quality, QualityLevel};
use crate::Result;

#[derive(Default)]
struct Flags {
    opened: AtomicBool,
    ended: AtomicBool,
    failed: AtomicBool,
    frame: AtomicBool,
    error: Mutex<String>,
}

pub(super) struct WinEngine {
    player: MediaPlayer,
    flags: Arc<Flags>,
    // HLS: the playlist is fetched in the background; the player gets it once ready.
    pending: Option<IAsyncOperation<AdaptiveMediaSourceCreationResult>>,
    // The texture last wrapped as a WinRT surface, so it is not wrapped again every frame.
    surface: Option<(ID3D11Texture2D, IDirect3DSurface)>,
}

fn is_playlist(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

const TICKS_PER_SECOND: f64 = 10_000_000.0;

impl WinEngine {
    pub(super) fn open(url: &str) -> Result<Self> {
        let flags = Arc::new(Flags::default());
        let player = MediaPlayer::new()?;
        player.SetIsVideoFrameServerEnabled(true)?;
        player.SetAutoPlay(true)?;
        {
            let f = Arc::clone(&flags);
            player.MediaOpened(&TypedEventHandler::<MediaPlayer, IInspectable>::new(move |_, _| {
                f.opened.store(true, Ordering::Release);
                Ok(())
            }))?;
            let f = Arc::clone(&flags);
            player.MediaEnded(&TypedEventHandler::<MediaPlayer, IInspectable>::new(move |_, _| {
                f.ended.store(true, Ordering::Release);
                Ok(())
            }))?;
            let f = Arc::clone(&flags);
            player.MediaFailed(&TypedEventHandler::<MediaPlayer, MediaPlayerFailedEventArgs>::new(move |_, args| {
                let detail = args
                    .as_ref()
                    .map(|a| {
                        let message = a.ErrorMessage().map(|m| m.to_string()).unwrap_or_default();
                        let code = a.ExtendedErrorCode().map(|c| format!("0x{:08X}", c.0 as u32)).unwrap_or_default();
                        format!("{message} {code}").trim().to_owned()
                    })
                    .unwrap_or_default();
                if let Ok(mut error) = f.error.lock() {
                    *error = detail;
                }
                f.failed.store(true, Ordering::Release);
                Ok(())
            }))?;
            let f = Arc::clone(&flags);
            player.VideoFrameAvailable(&TypedEventHandler::<MediaPlayer, IInspectable>::new(move |_, _| {
                f.frame.store(true, Ordering::Release);
                Ok(())
            }))?;
        }
        let uri = Uri::CreateUri(&HSTRING::from(url))?;
        let mut engine = Self { player, flags, pending: None, surface: None };
        if is_playlist(url) {
            engine.pending = Some(AdaptiveMediaSource::CreateFromUriAsync(&uri)?);
        } else {
            let source = MediaSource::CreateFromUri(&uri)?;
            engine.player.SetSource(&source)?;
        }
        Ok(engine)
    }

    // Hands the HLS source to the player once its playlist has been read.
    pub(super) fn poll(&mut self) {
        let Some(op) = &self.pending else { return };
        let status = op.Status().unwrap_or(AsyncStatus::Error);
        if status == AsyncStatus::Started {
            return;
        }
        let op = self.pending.take();
        let result = op.and_then(|op| op.GetResults().ok());
        let ready = result.as_ref().and_then(|r| {
            let status = r.Status().ok()?;
            if status != AdaptiveMediaSourceCreationStatus::Success {
                let code = r.ExtendedError().map(|c| format!("0x{:08X}", c.0 as u32)).unwrap_or_default();
                tracing::warn!("no se pudo abrir la lista HLS ({status:?} {code})");
                return None;
            }
            r.MediaSource().ok()
        });
        let opened = ready.and_then(|adaptive| {
            if let Ok(rates) = adaptive.AvailableBitrates() {
                tracing::info!("vídeo HLS abierto con {} calidades", rates.Size().unwrap_or(0));
            }
            // Which download went wrong, and what the server answered (first few only).
            let logged = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let _ = adaptive.DownloadFailed(&TypedEventHandler::<AdaptiveMediaSource, AdaptiveMediaSourceDownloadFailedEventArgs>::new(
                move |_, args| {
                    if logged.fetch_add(1, Ordering::AcqRel) < 6
                        && let Some(a) = args.as_ref()
                    {
                        let uri = a.ResourceUri().and_then(|u| u.AbsoluteUri()).map(|u| u.to_string()).unwrap_or_default();
                        let uri: String = uri.chars().take(200).collect();
                        let kind = a.ResourceType().map(|k| format!("{k:?}")).unwrap_or_default();
                        let status = a.HttpResponseMessage().and_then(|m| m.StatusCode()).map(|c| c.0).unwrap_or(0);
                        let error = a.ExtendedError().map(|e| format!("0x{:08X}", e.0 as u32)).unwrap_or_default();
                        tracing::warn!("descarga HLS fallida ({kind}, HTTP {status}, {error}): {uri}");
                        // What the server really sent: its size and the first bytes (diagnostics).
                        let body = a
                            .HttpResponseMessage()
                            .and_then(|m| m.Content())
                            .and_then(|c| c.ReadAsBufferAsync())
                            .and_then(|op| op.join());
                        if let Ok(buffer) = body {
                            let len = buffer.Length().unwrap_or(0);
                            let mut head = Vec::new();
                            if let Ok(reader) = windows::Storage::Streams::DataReader::FromBuffer(&buffer) {
                                for _ in 0..len.min(48) {
                                    head.push(reader.ReadByte().unwrap_or(0));
                                }
                            }
                            let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
                            let text: String = head.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }).collect();
                            tracing::warn!("respuesta de {len} bytes: {} | {text}", hex.join(" "));
                        }
                    }
                    Ok(())
                },
            ));
            let source = MediaSource::CreateFromAdaptiveMediaSource(&adaptive).ok()?;
            self.player.SetSource(&source).ok()
        });
        if opened.is_none() {
            self.flags.failed.store(true, Ordering::Release);
        }
    }

    pub(super) fn opened(&self) -> bool {
        self.flags.opened.load(Ordering::Acquire)
    }

    pub(super) fn ended(&self) -> bool {
        self.flags.ended.load(Ordering::Acquire)
    }

    pub(super) fn failed(&self) -> bool {
        self.flags.failed.load(Ordering::Acquire)
    }

    pub(super) fn error(&self) -> String {
        self.flags.error.lock().map(|e| e.clone()).unwrap_or_default()
    }

    fn state(&self) -> Option<MediaPlaybackState> {
        self.player.PlaybackSession().ok()?.PlaybackState().ok()
    }

    pub(super) fn paused(&self) -> bool {
        !matches!(self.state(), Some(MediaPlaybackState::Playing | MediaPlaybackState::Buffering | MediaPlaybackState::Opening))
    }

    pub(super) fn buffering(&self) -> bool {
        self.pending.is_some() || matches!(self.state(), Some(MediaPlaybackState::Buffering | MediaPlaybackState::Opening))
    }

    pub(super) fn duration(&self) -> f64 {
        let ticks = self.player.PlaybackSession().and_then(|s| s.NaturalDuration()).map_or(0, |d| d.Duration);
        // Live streams report a zero or huge duration.
        if ticks <= 0 || ticks == i64::MAX { f64::INFINITY } else { ticks as f64 / TICKS_PER_SECOND }
    }

    pub(super) fn position(&self) -> f64 {
        self.player.PlaybackSession().and_then(|s| s.Position()).map_or(0.0, |p| p.Duration as f64 / TICKS_PER_SECOND)
    }

    pub(super) fn video_size(&self) -> Option<(u32, u32)> {
        let session = self.player.PlaybackSession().ok()?;
        let (w, h) = (session.NaturalVideoWidth().ok()?, session.NaturalVideoHeight().ok()?);
        (w > 0 && h > 0).then_some((w, h))
    }

    pub(super) fn play(&self) {
        if self.ended() {
            self.seek(0.0);
            self.flags.ended.store(false, Ordering::Release);
        }
        let _ = self.player.Play();
    }

    pub(super) fn pause(&self) {
        let _ = self.player.Pause();
    }

    pub(super) fn set_volume(&self, volume: f32) {
        let _ = self.player.SetVolume(f64::from(volume.clamp(0.0, 1.0)));
    }

    pub(super) fn seek(&self, seconds: f64) {
        if let Ok(session) = self.player.PlaybackSession() {
            let _ = session.SetPosition(TimeSpan { Duration: (seconds.max(0.0) * TICKS_PER_SECOND) as i64 });
            self.flags.ended.store(false, Ordering::Release);
        }
    }

    pub(super) fn set_rate(&mut self, rate: f64) {
        if let Ok(session) = self.player.PlaybackSession() {
            let _ = session.SetPlaybackRate(rate);
        }
    }

    // Copies the newest frame into `texture`, inside `dst` (letterboxed). False when no new
    // frame arrived since the last call (unless `force`).
    pub(super) fn copy_frame(&mut self, texture: &ID3D11Texture2D, dst: (f32, f32, f32, f32), force: bool) -> bool {
        if !self.flags.frame.swap(false, Ordering::AcqRel) && !force {
            return false;
        }
        let surface = match &self.surface {
            Some((tex, surface)) if tex == texture => surface.clone(),
            _ => {
                let Some(surface) = wrap(texture) else { return false };
                self.surface = Some((texture.clone(), surface.clone()));
                surface
            }
        };
        let rect = WinRect { X: dst.0, Y: dst.1, Width: dst.2, Height: dst.3 };
        self.player.CopyFrameToVideoSurfaceWithTargetRectangle(&surface, rect).is_ok()
    }

    pub(super) fn close(&mut self) {
        self.pending = None;
        self.surface = None;
        let _ = self.player.Pause();
        let _ = self.player.Close();
    }
}

// The engine in use: RustPlay's own HLS player for playlists (YouTube and similar apps, which
// the Windows player cannot play), the Windows MediaPlayer for plain files.
pub(super) enum Engine {
    Native(Box<super::native::Native>),
    Windows(Box<WinEngine>),
}

macro_rules! delegate {
    ($self:ident, $method:ident($($arg:expr),*)) => {
        match $self {
            Engine::Native(e) => e.$method($($arg),*),
            Engine::Windows(e) => e.$method($($arg),*),
        }
    };
}

impl Engine {
    pub(super) fn open(url: &str, gpu: &std::sync::Arc<crate::media::gpu::GpuDevice>) -> Result<Self> {
        if is_playlist(url) && std::env::var_os("RUSTPLAY_WINDOWS_HLS").is_none() {
            return Ok(Self::Native(Box::new(super::native::Native::open(url, gpu)?)));
        }
        Ok(Self::Windows(Box::new(WinEngine::open(url)?)))
    }

    pub(super) fn poll(&mut self) {
        delegate!(self, poll())
    }
    pub(super) fn opened(&self) -> bool {
        delegate!(self, opened())
    }
    pub(super) fn ended(&self) -> bool {
        delegate!(self, ended())
    }
    pub(super) fn failed(&self) -> bool {
        delegate!(self, failed())
    }
    pub(super) fn error(&self) -> String {
        delegate!(self, error())
    }
    pub(super) fn paused(&self) -> bool {
        delegate!(self, paused())
    }
    pub(super) fn buffering(&self) -> bool {
        delegate!(self, buffering())
    }
    pub(super) fn duration(&self) -> f64 {
        delegate!(self, duration())
    }
    pub(super) fn position(&self) -> f64 {
        delegate!(self, position())
    }
    pub(super) fn video_size(&self) -> Option<(u32, u32)> {
        delegate!(self, video_size())
    }
    pub(super) fn play(&mut self) {
        delegate!(self, play())
    }
    pub(super) fn pause(&self) {
        delegate!(self, pause())
    }
    pub(super) fn seek(&self, seconds: f64) {
        delegate!(self, seek(seconds))
    }
    pub(super) fn set_volume(&self, volume: f32) {
        delegate!(self, set_volume(volume))
    }
    // Playback speed (the native engine stretches the sound; the Windows one uses PlaybackRate).
    pub(super) fn set_rate(&mut self, rate: f64) {
        delegate!(self, set_rate(rate))
    }
    // The reachable part of a live stream, in seconds.
    pub(super) fn live_window(&self) -> Option<(f64, f64)> {
        match self {
            Engine::Native(e) => e.live_window(),
            Engine::Windows(_) => None,
        }
    }
    // Qualities worst to best; empty when the engine cannot choose.
    pub(super) fn quality_levels(&self) -> Vec<QualityLevel> {
        match self {
            Engine::Native(e) => e.quality_levels(),
            Engine::Windows(_) => Vec::new(),
        }
    }
    pub(super) fn quality(&self) -> Quality {
        match self {
            Engine::Native(e) => e.quality(),
            Engine::Windows(_) => Quality::Auto,
        }
    }
    // Index (in `quality_levels`) of the quality being downloaded.
    pub(super) fn playing_rung(&self) -> usize {
        match self {
            Engine::Native(e) => e.playing_rung(),
            Engine::Windows(_) => 0,
        }
    }
    pub(super) fn set_quality(&self, quality: Quality) {
        if let Engine::Native(e) = self {
            e.set_quality(quality);
        }
    }
    pub(super) fn subtitle_names(&self) -> Vec<String> {
        match self {
            Engine::Native(e) => e.subtitle_names(),
            Engine::Windows(_) => Vec::new(),
        }
    }
    pub(super) fn subtitle_count(&self) -> usize {
        match self {
            Engine::Native(e) => e.subtitle_count(),
            Engine::Windows(_) => 0,
        }
    }
    pub(super) fn subtitle(&self) -> Option<usize> {
        match self {
            Engine::Native(e) => e.subtitle(),
            Engine::Windows(_) => None,
        }
    }
    pub(super) fn set_subtitle(&mut self, which: Option<usize>) {
        if let Engine::Native(e) = self {
            e.set_subtitle(which);
        }
    }
    pub(super) fn cue_text(&self) -> Option<String> {
        match self {
            Engine::Native(e) => e.cue_text(),
            Engine::Windows(_) => None,
        }
    }
    pub(super) fn copy_frame(&mut self, texture: &ID3D11Texture2D, dst: (f32, f32, f32, f32), size: (u32, u32), force: bool) -> bool {
        match self {
            Engine::Native(e) => e.copy_frame(texture, dst, size, force),
            Engine::Windows(e) => e.copy_frame(texture, dst, force),
        }
    }
    pub(super) fn close(&mut self) {
        delegate!(self, close())
    }
}

// The texture seen as a WinRT Direct3D surface, as CopyFrameToVideoSurface wants.
fn wrap(texture: &ID3D11Texture2D) -> Option<IDirect3DSurface> {
    let dxgi: IDXGISurface = texture.cast().ok()?;
    // SAFETY: superficie DXGI válida de una textura propia.
    let inspectable = unsafe { CreateDirect3D11SurfaceFromDXGISurface(&dxgi) }.ok()?;
    inspectable.cast().ok()
}

#[cfg(test)]
mod tests {
    use super::{WinEngine as Engine, is_playlist};

    // Plays an MP4 (path in RUSTPLAY_TEST_MP4, e.g. the clip the recorder test keeps with
    // RUSTPLAY_KEEP_MP4) and checks that a decoded picture reaches a texture of ours.
    #[test]
    #[ignore = "needs a GPU and an MP4 file"]
    fn plays_a_file_into_a_texture() {
        use crate::media::gpu::GpuDevice;
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
        let path = std::env::var("RUSTPLAY_TEST_MP4").expect("RUSTPLAY_TEST_MP4");
        crate::media::mf::ensure_started().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 320,
            Height: 240,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut tex = None;
        // SAFETY: descripción válida.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)).expect("textura") };
        let tex = tex.expect("textura");
        let url = format!("file:///{}", path.replace(std::path::MAIN_SEPARATOR, "/"));
        let mut engine = Engine::open(&url).expect("abrir");
        let started = std::time::Instant::now();
        let mut copied = false;
        while started.elapsed() < std::time::Duration::from_secs(10) {
            engine.poll();
            assert!(!engine.failed(), "falló: {}", engine.error());
            if engine.opened() && engine.copy_frame(&tex, (0.0, 0.0, 320.0, 240.0), false) {
                copied = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(copied, "no llegó ninguna imagen (abierto: {})", engine.opened());
        assert_eq!(engine.video_size(), Some((320, 240)));
        let image = crate::media::screenshot::read_texture(&gpu, &tex).expect("leer");
        let lit = image.pixels.chunks_exact(4).filter(|p| p[0] > 20 || p[1] > 20 || p[2] > 20).count();
        assert!(lit > 1000, "la imagen está vacía ({lit} píxeles)");
        engine.close();
    }

    #[test]
    fn playlists_are_told_apart_from_files() {
        assert!(is_playlist("http://127.0.0.1:7000/master.m3u8"));
        assert!(is_playlist("https://x/y/index.M3U8?token=1"));
        assert!(!is_playlist("https://x/video.mp4"));
        assert!(!is_playlist("https://x/m3u8/video.mp4"));
    }
}

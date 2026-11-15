// Recording and screenshots of the mirrored screen. The video thread owns a `Controller`; the audio
// thread and the app reach the running recording through the shared `Capture`.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rustplay_core::media::recorder::{
    AudioSpec, GOP_CACHE_BYTES, GopCache, Recorder, RecorderConfig, RecorderEvent, default_recordings_dir,
    default_screenshots_dir, is_keyframe, timestamp_stem, unique_path,
};
use rustplay_core::media::gpu::GpuDevice;
use rustplay_core::media::reencode::Reencoder;
use rustplay_core::media::screenshot;
use rustplay_core::media::video::DecodedFrame;
use rustplay_core::media::video::Presenter;
use tokio::sync::mpsc as async_mpsc;

use crate::{Live, RxEvent};

// Where captures go and whether recordings include sound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOptions {
    pub recordings_dir: PathBuf,
    pub screenshots_dir: PathBuf,
    pub record_audio: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            recordings_dir: default_recordings_dir(),
            screenshots_dir: default_screenshots_dir(),
            record_audio: true,
        }
    }
}

// State shared between the mirror window, the audio renderer and the app.
#[derive(Default)]
pub(crate) struct Capture {
    record_request: AtomicBool,
    screenshot_request: AtomicBool,
    mirroring: AtomicBool,
    recorder: RwLock<Option<Arc<Recorder>>>,
    audio: Mutex<Option<AudioSpec>>,
}

impl Capture {
    pub fn set_mirroring(&self, on: bool) {
        self.mirroring.store(on, Ordering::Release);
        if !on {
            self.record_request.store(false, Ordering::Release);
            self.screenshot_request.store(false, Ordering::Release);
        }
    }

    // Asks the mirror window to start or stop recording; false when nothing is being mirrored.
    pub fn request_record(&self) -> bool {
        let mirroring = self.mirroring.load(Ordering::Acquire);
        if mirroring {
            self.record_request.store(true, Ordering::Release);
        }
        mirroring
    }

    pub fn request_screenshot(&self) -> bool {
        let mirroring = self.mirroring.load(Ordering::Acquire);
        if mirroring {
            self.screenshot_request.store(true, Ordering::Release);
        }
        mirroring
    }

    pub fn is_mirroring(&self) -> bool {
        self.mirroring.load(Ordering::Acquire)
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.recorder.read().ok()?.as_ref().map(|r| r.started().elapsed())
    }

    pub fn set_audio(&self, spec: Option<AudioSpec>) {
        if let Ok(mut audio) = self.audio.lock() {
            *audio = spec;
        }
    }

    fn audio(&self) -> Option<AudioSpec> {
        self.audio.lock().ok().and_then(|a| *a)
    }

    // Called by the audio renderer with every decoded packet; copies it only while recording.
    pub fn push_audio(&self, spec: AudioSpec, pcm: &[i16]) {
        if let Ok(slot) = self.recorder.read()
            && let Some(recorder) = slot.as_ref()
            && recorder.audio() == Some(spec)
        {
            recorder.push_audio(pcm);
        }
    }

    fn set_recorder(&self, recorder: Option<Arc<Recorder>>) {
        if let Ok(mut slot) = self.recorder.write() {
            *slot = recorder;
        }
    }

    fn take_record_request(&self) -> bool {
        self.record_request.swap(false, Ordering::AcqRel)
    }

    fn take_screenshot_request(&self) -> bool {
        self.screenshot_request.swap(false, Ordering::AcqRel)
    }
}

// Owned by the video thread.
pub(crate) struct Controller {
    gop: GopCache,
    active: Option<Arc<Recorder>>,
    // Stopped recorders whose files are still being closed (dropping one waits for it).
    finishing: Vec<Arc<Recorder>>,
    failed: Arc<AtomicBool>,
    events: async_mpsc::Sender<RxEvent>,
    // Set when the running recording is made by re-encoding the pictures (see `start`).
    reencode: bool,
    reencoder: Option<Reencoder>,
}

// A recording may begin with the pictures since the device's last keyframe only while that is
// recent (about this many frames); older than that, the pictures would replay from far back.
const MAX_PREROLL_FRAMES: usize = 90;

// Serialises file naming and writing of screenshots taken in quick succession.
static SHOT_LOCK: Mutex<()> = Mutex::new(());

impl Controller {
    pub fn new(events: async_mpsc::Sender<RxEvent>) -> Self {
        Self {
            gop: GopCache::new(GOP_CACHE_BYTES),
            active: None,
            finishing: Vec::new(),
            failed: Arc::new(AtomicBool::new(false)),
            events,
            reencode: false,
            reencoder: None,
        }
    }

    // Every received access unit, in order, shown or not.
    pub fn observe(&mut self, unit: &[u8], pts: i64) {
        self.gop.observe(unit, pts, is_keyframe(unit));
        if !self.reencode
            && let Some(recorder) = &self.active
        {
            recorder.push_video(unit, pts);
        }
    }

    // Every decoded picture, in order. While a recording is made by re-encoding, they feed it.
    pub fn observe_frame(&mut self, gpu: &Arc<GpuDevice>, frame: &DecodedFrame) {
        let Some(recorder) = self.active.clone().filter(|_| self.reencode) else {
            self.reencoder = None;
            return;
        };
        if self.reencoder.as_ref().is_none_or(|r| r.size() != (frame.width & !1, frame.height & !1)) {
            match Reencoder::new(gpu, frame.width, frame.height) {
                Ok(reencoder) => self.reencoder = Some(reencoder),
                Err(e) => {
                    tracing::warn!("no se pudo preparar la grabación con recodificación: {e}");
                    self.fail_recording(&e.to_string());
                    return;
                }
            }
        }
        let Some(reencoder) = self.reencoder.as_mut() else { return };
        let mut sink = |data: &[u8], pts: i64| {
            recorder.push_video(data, pts);
        };
        if let Err(e) = reencoder.encode(frame, &mut sink) {
            tracing::warn!("error al recodificar para la grabación: {e}");
            self.fail_recording(&e.to_string());
        }
    }

    fn fail_recording(&mut self, message: &str) {
        self.reencoder = None;
        self.failed.store(true, Ordering::Release);
        let _ = self.events.try_send(RxEvent::RecordingFailed { message: message.to_owned() });
    }

    // Handles button presses and requests from the app, and closes finished recorders.
    pub fn tick(&mut self, live: &Live, presenter: &mut Presenter) {
        self.finishing.retain(|r| !r.is_finished());
        if self.active.is_some() && self.failed.load(Ordering::Acquire) {
            self.stop(live, presenter);
        }
        if presenter.take_record_request() | live.capture.take_record_request() {
            if self.active.is_some() {
                self.stop(live, presenter);
            } else {
                self.start(live, presenter);
            }
        }
        if presenter.take_screenshot_request() | live.capture.take_screenshot_request() {
            self.screenshot(live, presenter);
        }
    }

    fn start(&mut self, live: &Live, presenter: &mut Presenter) {
        let options = live.get().capture;
        let audio = if options.record_audio { live.capture.audio() } else { None };
        let events = self.events.clone();
        // Starting from the pictures since the last keyframe is invisible when that keyframe is
        // recent; otherwise the recording is re-encoded from this moment.
        let preroll = self.gop.snapshot().filter(|p| p.frames() <= MAX_PREROLL_FRAMES);
        self.reencode = preroll.is_none();
        self.reencoder = None;
        if self.reencode {
            tracing::info!("grabación: no hay un fotograma clave reciente; se recodifica desde ahora");
        }
        self.failed.store(false, Ordering::Release);
        let failed = Arc::clone(&self.failed);
        let config = RecorderConfig {
            folder: options.recordings_dir,
            stem: timestamp_stem(),
            audio,
            preroll,
        };
        let started = Recorder::start(config, move |event| {
            let event = match event {
                RecorderEvent::Started(_) => return,
                RecorderEvent::Saved(path) => RxEvent::RecordingSaved { path },
                RecorderEvent::Failed(message) => {
                    failed.store(true, Ordering::Release);
                    RxEvent::RecordingFailed { message }
                }
            };
            let _ = events.try_send(event);
        });
        match started {
            Ok(recorder) => {
                let recorder = Arc::new(recorder);
                presenter.set_recording(Some(recorder.started()));
                live.capture.set_recorder(Some(Arc::clone(&recorder)));
                tracing::info!("grabación de la duplicación iniciada (audio: {})", if audio.is_some() { "sí" } else { "no" });
                self.active = Some(recorder);
                let _ = self.events.try_send(RxEvent::RecordingState { active: true });
            }
            Err(e) => {
                tracing::warn!("no se pudo iniciar la grabación: {e}");
                let _ = self.events.try_send(RxEvent::RecordingFailed { message: e.to_string() });
            }
        }
    }

    fn stop(&mut self, live: &Live, presenter: &mut Presenter) {
        live.capture.set_recorder(None);
        presenter.set_recording(None);
        self.reencoder = None;
        self.reencode = false;
        if let Some(recorder) = self.active.take() {
            recorder.stop();
            self.finishing.push(recorder);
            let _ = self.events.try_send(RxEvent::RecordingState { active: false });
        }
    }

    fn screenshot(&mut self, live: &Live, presenter: &mut Presenter) {
        let folder = live.get().capture.screenshots_dir;
        let image = match presenter.capture_frame() {
            Ok(Some(image)) => image,
            Ok(None) => {
                tracing::warn!("captura de pantalla: todavía no hay ninguna imagen");
                let _ = self.events.try_send(RxEvent::ScreenshotFailed { message: "todavía no hay ninguna imagen".into() });
                return;
            }
            Err(e) => {
                tracing::warn!("no se pudo leer la imagen para la captura: {e}");
                let _ = self.events.try_send(RxEvent::ScreenshotFailed { message: e.to_string() });
                return;
            }
        };
        let events = self.events.clone();
        let spawned = std::thread::Builder::new().name("rustplay-screenshot".into()).spawn(move || {
            let _guard = SHOT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let path = unique_path(&folder, &timestamp_stem(), "png");
            match screenshot::save_png(&path, &image) {
                Ok(()) => {
                    let _ = events.try_send(RxEvent::ScreenshotSaved { path });
                }
                Err(e) => {
                    tracing::warn!("no se pudo guardar la captura: {e}");
                    let _ = events.try_send(RxEvent::ScreenshotFailed { message: e.to_string() });
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("no se pudo crear el hilo de la captura: {e}");
        }
    }

    // Closes the recording when the window goes away; waits until the files are written.
    pub fn finish(&mut self, live: &Live) {
        live.capture.set_recorder(None);
        self.reencoder = None;
        self.reencode = false;
        if let Some(recorder) = self.active.take() {
            recorder.finish();
            let _ = self.events.try_send(RxEvent::RecordingState { active: false });
        }
        self.finishing.clear();
    }
}

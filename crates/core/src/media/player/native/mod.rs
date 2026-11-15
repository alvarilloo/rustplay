// RustPlay's own HLS player. It downloads the playlists and segments (WinHTTP), separates video
// and audio (MPEG-TS or fragmented MP4), decodes H.264 with the same GPU decoder the screen
// mirroring uses and AAC with the Windows decoder, plays the sound through the audio renderer and
// keeps the picture in step with it. No external player is needed.
//
// Threads of one playback ("pipeline"): a feeder per stream downloads and demuxes segments, one
// decoder thread per stream decodes. Seeking and restarting build a new pipeline; the old one is
// told to stop and ends by itself, so closing never waits for the network.
//
// The video feeder picks the quality (see abr.rs) at every segment boundary and follows live
// playlists by reloading them; subtitles come from a separate loader thread (vtt.rs); the audio
// thread stretches the sound for playback speeds other than 1x (stretch.rs).

mod abr;
mod blit;
mod http;
mod mp4;
mod pcm;
mod playlist;
mod stretch;
mod ts;
mod vtt;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub(super) use self::abr::Quality;
pub(super) use self::stretch::{SPEEDS, step_speed};

use self::abr::{Controller, Decision, Rung};
use self::mp4::Mp4Demuxer;
use self::playlist::{Media, SubtitleTrack, Timeline};
use self::ts::{Demuxer, Kind, Packet};
use crate::Result;
use crate::media::audio::{AudioCodec, AudioFormat, Renderer, create_decoder};
use crate::media::gpu::GpuDevice;
use crate::media::video::{DecodedFrame, H264Decoder};

const MAX_HEIGHT: u32 = 1080;
const QUEUED_FRAMES: usize = 3;
const QUEUED_VIDEO_PACKETS: usize = 2048;
const QUEUED_AUDIO_PACKETS: usize = 512;
const SEGMENT_RETRIES: u32 = 3;
const NOT_STARTED: i64 = -1;
const AUDIO_PATIENCE: Duration = Duration::from_secs(4);
// How much media (microseconds) a feeder downloads ahead of the playing position.
const AHEAD_US: i64 = 24_000_000;
// A live stream starts this many target durations behind the newest segment.
const LIVE_EDGE_SEGMENTS: f64 = 3.0;
// Consecutive failed reloads of a live playlist before giving up.
const REFRESH_FAILURES: u32 = 6;

struct Item {
    // Media time in microseconds.
    time_us: i64,
    data: Vec<u8>,
}

struct Frame {
    time_us: i64,
    frame: DecodedFrame,
}

// SAFETY: the picture is a decoder output (a D3D11 texture and its Media Foundation sample) on a
// device created with multithread protection; it is only handed over through the queue.
unsafe impl Send for Frame {}

struct Setup {
    // The playable variants, worst to best; empty when the address is a media playlist itself.
    ladder: Vec<playlist::Variant>,
    rungs: Vec<Rung>,
    // The ladder index (or 0) the playback starts with.
    initial: usize,
    // The playlist of `initial` (or the address itself) as it was when the stream was opened.
    video_url: String,
    video: Arc<Media>,
    audio_url: Option<String>,
    audio: Option<Arc<Media>>,
    // The audio is expected in the video segments or in its own playlist.
    has_audio: bool,
    subtitles: Vec<SubtitleTrack>,
}

impl Setup {
    fn playlist_url(&self, rung: usize) -> &str {
        self.ladder.get(rung).map_or(self.video_url.as_str(), |v| v.uri.as_str())
    }
}

#[derive(Default)]
struct Failure(Mutex<Option<String>>);

impl Failure {
    fn set(&self, message: String) {
        if let Ok(mut m) = self.0.lock()
            && m.is_none()
        {
            tracing::warn!("el reproductor HLS falló: {message}");
            *m = Some(message);
        }
    }

    fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|m| m.clone())
    }
}

// The media time line of each stream; the feeders extend it as live playlists grow.
struct Timelines {
    video: Timeline,
    audio: Option<Timeline>,
}

// State of one playback from a start time; shared by its threads.
struct Pipe {
    created: Instant,
    stop: AtomicBool,
    paused: AtomicBool,
    start_us: i64,
    frames: Mutex<VecDeque<Frame>>,
    space: Condvar,
    video_done: AtomicBool,
    audio_done: AtomicBool,
    has_audio: AtomicBool,
    audio_clock_us: AtomicI64,
    wall: Mutex<Wall>,
    size: AtomicU32,
    failure: Arc<Failure>,
    volume: Arc<AtomicU32>,
    // Playback speed as `f32` bits.
    speed: Arc<AtomicU32>,
}

// Clock for streams without sound.
#[derive(Default)]
struct Wall {
    started: Option<Instant>,
    // Media microseconds that went by before `started`.
    before_us: f64,
}

impl Pipe {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn speed(&self) -> f64 {
        f64::from(f32::from_bits(self.speed.load(Ordering::Acquire)))
    }

    fn now_us(&self) -> i64 {
        if self.has_audio.load(Ordering::Acquire) {
            let clock = self.audio_clock_us.load(Ordering::Acquire);
            return if clock == NOT_STARTED { self.start_us } else { clock };
        }
        let speed = self.speed();
        let wall = self.wall.lock().map(|w| w.before_us + w.started.map_or(0.0, |s| s.elapsed().as_micros() as f64 * speed)).unwrap_or_default();
        self.start_us + wall as i64
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        let speed = self.speed();
        if let Ok(mut wall) = self.wall.lock() {
            if paused {
                if let Some(since) = wall.started.take() {
                    wall.before_us += since.elapsed().as_micros() as f64 * speed;
                }
            } else if wall.started.is_none() && self.size.load(Ordering::Acquire) != 0 {
                wall.started = Some(Instant::now());
            }
        }
    }

    // Changes the speed without making the wall clock jump.
    fn set_speed(&self, speed: f32) {
        let old = self.speed();
        if let Ok(mut wall) = self.wall.lock() {
            if let Some(since) = wall.started {
                wall.before_us += since.elapsed().as_micros() as f64 * old;
                wall.started = Some(Instant::now());
            }
            self.speed.store(speed.to_bits(), Ordering::Release);
        }
    }

    fn start_wall(&self) {
        if let Ok(mut wall) = self.wall.lock()
            && wall.started.is_none()
            && !self.paused.load(Ordering::Acquire)
        {
            wall.started = Some(Instant::now());
        }
    }
}

fn send(tx: &SyncSender<Item>, mut item: Item, pipe: &Pipe) -> bool {
    loop {
        if pipe.stopped() {
            return false;
        }
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Full(back)) => {
                item = back;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn download(url: &str, pipe: &Pipe) -> std::result::Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 0..SEGMENT_RETRIES {
        if pipe.stopped() {
            return Err("detenido".into());
        }
        match http::get(url) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                tracing::debug!("descarga fallida (intento {}): {e}", attempt + 1);
                last = e;
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
    Err(last)
}

// Transport stream or fragmented MP4, depending on the playlist.
enum Demux {
    Ts(Demuxer),
    Mp4(Mp4Demuxer),
}

impl Demux {
    fn push(&mut self, bytes: &[u8]) -> Vec<Packet> {
        match self {
            Demux::Ts(d) => d.push_segment(bytes),
            Demux::Mp4(d) => d.push_segment(bytes),
        }
    }

    fn has_audio(&self) -> bool {
        match self {
            Demux::Ts(d) => d.has_audio(),
            Demux::Mp4(d) => d.has_audio(),
        }
    }

    fn unsupported(&self) -> String {
        match self {
            Demux::Ts(d) => format!("{:?}", d.unsupported),
            Demux::Mp4(_) => "[]".into(),
        }
    }
}

// A media playlist being followed.
struct Track {
    rung: usize,
    url: String,
    media: Arc<Media>,
    demux: Demux,
}

// Downloads and demuxes the segments of one stream from sequence number `first_sequence`.
// `video` is Some for the feeder that defines the time origin (and receives the video); packets go
// to the matching channel.
struct Feeder {
    pipe: Arc<Pipe>,
    shared: Arc<Shared>,
    setup: Arc<Setup>,
    // The ladder index to start with (video feeder).
    rung: usize,
    first_sequence: u64,
    video: Option<SyncSender<Item>>,
    audio: Option<SyncSender<Item>>,
}

impl Feeder {
    fn is_video(&self) -> bool {
        self.video.is_some()
    }

    fn with_timeline<R>(&self, f: impl FnOnce(&mut Timeline) -> R) -> Option<R> {
        let mut timelines = self.shared.timelines.lock().ok()?;
        let timeline = if self.is_video() { &mut timelines.video } else { timelines.audio.as_mut()? };
        Some(f(timeline))
    }

    // A playlist; finished ones are kept for later quality switches.
    fn media_for(&self, url: &str, fresh: bool) -> std::result::Result<Arc<Media>, String> {
        if !fresh && let Some(media) = self.shared.playlists.lock().ok().and_then(|c| c.get(url).cloned()) {
            return Ok(media);
        }
        let bytes = download(url, &self.pipe)?;
        let text = String::from_utf8_lossy(&bytes);
        if !text.trim_start().starts_with("#EXTM3U") {
            return Err("la lista de reproducción no es válida".into());
        }
        let media = Arc::new(playlist::parse_media(&text, url));
        if media.ended
            && let Ok(mut cache) = self.shared.playlists.lock()
        {
            cache.insert(url.to_owned(), Arc::clone(&media));
        }
        Ok(media)
    }

    fn open_track(&self, rung: usize, url: &str, media: Arc<Media>) -> std::result::Result<Track, String> {
        let demux = match &media.map {
            Some(init) => {
                let bytes = download(init, &self.pipe)?;
                Demux::Mp4(Mp4Demuxer::from_init(&bytes)?)
            }
            None => Demux::Ts(Demuxer::default()),
        };
        Ok(Track { rung, url: url.to_owned(), media, demux })
    }

    // Waits until the playing position is close enough to the downloaded media; false when stopped.
    fn wait_ahead(&self, last_us: i64) -> bool {
        while last_us - self.pipe.now_us() > AHEAD_US {
            if self.pipe.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.pipe.stopped()
    }

    // The quality for the next segment.
    fn choose_rung(&self, controller: &mut Controller, current: usize, last_us: i64) -> usize {
        let n = self.setup.rungs.len();
        match self.shared.quality() {
            Quality::Fixed(i) => i.min(n.saturating_sub(1)),
            Quality::Auto => {
                let buffer = (self.pipe.size.load(Ordering::Acquire) != 0).then(|| (last_us - self.pipe.now_us()) as f64 / 1e6);
                match controller.next(&self.setup.rungs, current, buffer) {
                    Decision::Switch(rung) => rung,
                    Decision::Stay => current,
                }
            }
        }
    }

    fn switch_to(&self, track: &mut Track, rung: usize, sequence: u64, throughput_bps: Option<f64>) {
        let url = self.setup.playlist_url(rung).to_owned();
        let opened = self.media_for(&url, !self.setup.video.ended).and_then(|media| {
            if media.ended && media.segment(sequence).is_none() {
                return Err("la otra calidad no tiene ese fragmento".to_owned());
            }
            self.open_track(rung, &url, media)
        });
        match opened {
            Ok(new) => {
                let (from, to) = (&self.setup.ladder[track.rung], &self.setup.ladder[rung]);
                tracing::info!(
                    "HLS: calidad {}p -> {}p ({} -> {} kbps, descarga medida {:.0} kbps)",
                    from.height,
                    to.height,
                    from.bandwidth / 1000,
                    to.bandwidth / 1000,
                    throughput_bps.unwrap_or(0.0) / 1000.0
                );
                self.with_timeline(|t| t.update(&new.media));
                self.shared.rung.store(rung, Ordering::Release);
                *track = new;
            }
            Err(e) => tracing::warn!("HLS: no se pudo cambiar de calidad: {e}"),
        }
    }

    // The next segment to download, reloading a live playlist while it has none; None at the end
    // of the stream (or when stopped or when the playlist cannot be reloaded).
    fn next_segment(&self, track: &mut Track, sequence: &mut u64) -> Option<playlist::Segment> {
        let mut failures = 0;
        let mut waited_for_news = false;
        loop {
            if self.pipe.stopped() {
                return None;
            }
            if *sequence < track.media.first_sequence {
                tracing::warn!("HLS: el reproductor se quedó atrás de la ventana en directo: salta del fragmento {} al {}", *sequence, track.media.first_sequence);
                *sequence = track.media.first_sequence;
            }
            if let Some(segment) = track.media.segment(*sequence) {
                return Some(segment.clone());
            }
            if track.media.ended {
                self.shared.ended.store(true, Ordering::Release);
                return None;
            }
            // Live: look for news about every half target duration.
            let wait = Duration::from_secs_f64((track.media.target() / 2.0).clamp(0.25, 5.0));
            let until = Instant::now() + wait;
            while Instant::now() < until {
                if self.pipe.stopped() {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            match self.media_for(&track.url, true) {
                Ok(media) => {
                    failures = 0;
                    self.with_timeline(|t| t.update(&media));
                    if media.map != track.media.map {
                        tracing::debug!("HLS: la lista en directo cambió su sección de inicialización");
                    }
                    if !waited_for_news {
                        waited_for_news = true;
                        tracing::debug!("HLS: lista en directo recargada ({} fragmentos desde el {})", media.segments.len(), media.first_sequence);
                    }
                    track.media = media;
                }
                Err(e) => {
                    failures += 1;
                    if failures >= REFRESH_FAILURES {
                        self.pipe.failure.set(format!("no se pudo recargar la lista en directo: {e}"));
                        return None;
                    }
                }
            }
        }
    }

    fn run(self) {
        let pipe = Arc::clone(&self.pipe);
        let setup = Arc::clone(&self.setup);
        let video = self.is_video();
        let name = if video { "vídeo" } else { "audio" };
        let live = !setup.video.ended;
        let (url, media) = if video {
            let url = setup.playlist_url(self.rung).to_owned();
            let media = if !live && self.rung == setup.initial { Ok(Arc::clone(&setup.video)) } else { self.media_for(&url, live) };
            (url, media)
        } else {
            let url = setup.audio_url.clone().unwrap_or_default();
            let media = match &setup.audio {
                Some(m) if !live => Ok(Arc::clone(m)),
                _ => self.media_for(&url, live),
            };
            (url, media)
        };
        let opened = media.and_then(|media| {
            self.with_timeline(|t| t.update(&media));
            self.open_track(self.rung, &url, media)
        });
        let mut track = match opened {
            Ok(track) => track,
            Err(e) => {
                if !pipe.stopped() {
                    pipe.failure.set(format!("no se pudo abrir la lista de {name}: {e}"));
                }
                return;
            }
        };
        let mut sequence = self.first_sequence;
        // (pts of the first packet of this stream in 100 ns, media time in microseconds): each
        // stream is anchored to the start of its own segment, so streams whose timestamps start
        // from different values still line up.
        let mut origin: Option<(i64, i64)> = None;
        let mut controller = Controller::default();
        let mut last_us = pipe.start_us;
        let mut first = true;
        loop {
            if !self.wait_ahead(last_us) {
                return;
            }
            if video && self.setup.rungs.len() > 1 {
                let wanted = self.choose_rung(&mut controller, track.rung, last_us);
                if wanted != track.rung {
                    self.switch_to(&mut track, wanted, sequence, None);
                }
            }
            let Some(segment) = self.next_segment(&mut track, &mut sequence) else { return };
            let started = Instant::now();
            let bytes = match download(&segment.url, &pipe) {
                Ok(bytes) => bytes,
                Err(e) => {
                    if !pipe.stopped() {
                        pipe.failure.set(format!("no se pudo descargar un fragmento de {name}: {e}"));
                    }
                    return;
                }
            };
            if video {
                controller.record(bytes.len(), started.elapsed().as_secs_f64());
            }
            let packets = track.demux.push(&bytes);
            let start_us = self.with_timeline(|t| t.start_of(sequence)).flatten().map_or(0, |s| (s * 1e6) as i64);
            let kind_of = |k: Kind| packets.iter().find(|p| p.kind == k).and_then(|p| p.pts);
            if first {
                let count = |k: Kind| packets.iter().filter(|p| p.kind == k).count();
                tracing::info!(
                    "HLS {name}: primer fragmento de {} bytes con {} paquetes de vídeo (PTS inicial {:?}) y {} de audio (PTS inicial {:?}); otros tipos: {}",
                    bytes.len(),
                    count(Kind::Video),
                    kind_of(Kind::Video),
                    count(Kind::Audio),
                    kind_of(Kind::Audio),
                    track.demux.unsupported()
                );
            }
            if first || segment.discontinuity {
                let anchor = if video { kind_of(Kind::Video) } else { kind_of(Kind::Audio) };
                match anchor {
                    Some(pts) => {
                        origin = Some((pts, start_us));
                        if video && let Ok(mut shared_origin) = self.shared.origin.lock() {
                            *shared_origin = origin;
                        }
                    }
                    None if first && video => {
                        pipe.failure.set(format!("el fragmento no contiene vídeo H.264 (otros tipos: {})", track.demux.unsupported()));
                        return;
                    }
                    None if first => {
                        tracing::warn!("el fragmento de audio no contiene audio AAC: se reproduce solo la imagen");
                        pipe.has_audio.store(false, Ordering::Release);
                        pipe.audio_done.store(true, Ordering::Release);
                        return;
                    }
                    None => {}
                }
                // Audio expected inside the video segments but not there: play without it.
                if first && video && self.audio.is_some() && !track.demux.has_audio() {
                    tracing::info!("los fragmentos no llevan audio: se reproduce solo la imagen");
                    pipe.has_audio.store(false, Ordering::Release);
                    pipe.audio_done.store(true, Ordering::Release);
                }
            }
            first = false;
            let Some((origin_pts, origin_us)) = origin else { return };
            for packet in packets {
                let Some(pts) = packet.pts else { continue };
                let time_us = origin_us + (pts - origin_pts) / 10;
                let tx = match packet.kind {
                    Kind::Video => self.video.as_ref(),
                    Kind::Audio => self.audio.as_ref(),
                };
                if (packet.kind == Kind::Video) == video {
                    last_us = last_us.max(time_us);
                }
                if let Some(tx) = tx
                    && !send(tx, Item { time_us, data: packet.data }, &pipe)
                {
                    return;
                }
            }
            // A segment without packets still moves the lookahead along.
            last_us = last_us.max(start_us + (segment.duration * 1e6) as i64);
            sequence += 1;
        }
    }
}

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn video_thread(pipe: Arc<Pipe>, gpu: Arc<GpuDevice>, rx: Receiver<Item>, shared: Arc<Shared>) {
    let result = crate::media::init_thread().and_then(|()| H264Decoder::new(Arc::clone(&gpu)));
    let mut decoder = match result {
        Ok(decoder) => decoder,
        Err(e) => {
            pipe.failure.set(format!("no se pudo iniciar el decodificador de vídeo: {e}"));
            return;
        }
    };
    let mut errors = 0u32;
    let mut last_size = (0, 0);
    while !pipe.stopped() {
        let item = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(item) => item,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match decoder.decode(&item.data, item.time_us * 10) {
            Ok(Some(frame)) => {
                let time_us = frame.pts / 10;
                // Pictures before the start point (decoded only to reach the target) are dropped.
                if time_us + 20_000 < pipe.start_us {
                    continue;
                }
                if last_size != (frame.width, frame.height) {
                    if last_size != (0, 0) {
                        tracing::info!("vídeo HLS: la imagen pasa de {}x{} a {}x{}", last_size.0, last_size.1, frame.width, frame.height);
                    }
                    last_size = (frame.width, frame.height);
                }
                shared.width.store(frame.width, Ordering::Release);
                shared.height.store(frame.height, Ordering::Release);
                // The window keeps the size of the first picture (or of the best quality):
                // later quality switches only rescale the picture inside it.
                let _ = shared.display.compare_exchange(0, pack_size(frame.width, frame.height), Ordering::AcqRel, Ordering::Acquire);
                pipe.size.store(1, Ordering::Release);
                let mut frames = match pipe.frames.lock() {
                    Ok(frames) => frames,
                    Err(_) => return,
                };
                while frames.len() >= QUEUED_FRAMES && !pipe.stopped() {
                    frames = match pipe.space.wait_timeout(frames, Duration::from_millis(20)) {
                        Ok((frames, _)) => frames,
                        Err(_) => return,
                    };
                }
                if pipe.stopped() {
                    return;
                }
                // Keeps the queue in display order even if the decoder reorders pictures.
                let at = frames.iter().position(|f| f.time_us > time_us).unwrap_or(frames.len());
                frames.insert(at, Frame { time_us, frame });
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                if errors == 1 || errors % 100 == 0 {
                    tracing::warn!("error al decodificar el vídeo ({errors}): {e}");
                }
            }
        }
    }
    pipe.video_done.store(true, Ordering::Release);
}

// Media time that has been played, from the frames the device has taken.
fn publish_clock(pipe: &Pipe, renderer: &Renderer, base: Option<i64>, pushed: u64, ledger: &mut stretch::Ledger) {
    if let Some(base) = base {
        let played = pushed.saturating_sub(renderer.buffered_frames() as u64);
        pipe.audio_clock_us.store(base + ledger.media_us(played) as i64, Ordering::Release);
    }
}

fn audio_thread(pipe: Arc<Pipe>, rx: Receiver<Item>) {
    let mut renderer = match crate::media::init_thread().and_then(|()| Renderer::start(200)) {
        Ok(renderer) => renderer,
        Err(e) => {
            tracing::warn!("sin audio para el vídeo ({e}): se sigue solo con la imagen");
            pipe.has_audio.store(false, Ordering::Release);
            pipe.start_wall();
            // Keep the channel drained so the feeder never blocks on it.
            while !pipe.stopped() {
                if let Err(RecvTimeoutError::Disconnected) = rx.recv_timeout(Duration::from_millis(50)) {
                    break;
                }
            }
            pipe.audio_done.store(true, Ordering::Release);
            return;
        }
    };
    let mut decoder: Option<(Box<dyn crate::media::audio::Decoder>, u32, u16, pcm::Resampler)> = None;
    let mut out = vec![0i16; 16_384];
    let (mut stereo, mut resampled, mut stretched) = (Vec::new(), Vec::new(), Vec::new());
    let mut stretcher = stretch::Stretcher::new(1.0);
    let mut stretching = false;
    let mut ledger = stretch::Ledger::default();
    let mut base_us: Option<i64> = None;
    let mut pushed: u64 = 0;
    let mut errors = 0u32;
    let mut announced = false;

    'outer: while !pipe.stopped() {
        renderer.set_volume(f32::from_bits(pipe.volume.load(Ordering::Acquire)));
        let item = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(item) => item,
            Err(RecvTimeoutError::Timeout) => {
                publish_clock(&pipe, &renderer, base_us, pushed, &mut ledger);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut time_us = item.time_us;
        for frame in ts::split_adts(&item.data) {
            let frame_us = 1024 * 1_000_000 / i64::from(frame.sample_rate);
            let this_us = time_us;
            time_us += frame_us;
            if this_us + frame_us < pipe.start_us {
                continue;
            }
            if decoder.as_ref().is_none_or(|(_, rate, channels, _)| *rate != frame.sample_rate || *channels != frame.channels) {
                let format = AudioFormat {
                    codec: AudioCodec::AacLc,
                    sample_rate: frame.sample_rate,
                    channels: frame.channels.clamp(1, 2),
                    frames_per_packet: 1024,
                };
                match create_decoder(&format) {
                    Ok(d) => decoder = Some((d, frame.sample_rate, frame.channels, pcm::Resampler::new(frame.sample_rate))),
                    Err(e) => {
                        pipe.failure.set(format!("no se pudo iniciar el decodificador de audio: {e}"));
                        return;
                    }
                }
            }
            let Some((decoder, _, channels, resampler)) = decoder.as_mut() else { continue };
            let produced = match decoder.decode(frame.raw, &mut out) {
                Ok(frames) => frames * 2,
                Err(e) => {
                    errors += 1;
                    if errors == 1 || errors % 100 == 0 {
                        tracing::warn!("error al decodificar el audio ({errors}): {e}");
                    }
                    continue;
                }
            };
            if !announced {
                announced = true;
                tracing::info!(
                    "audio: AAC de {} Hz y {} canales; primer fotograma a los {:.2} s, {} muestras decodificadas",
                    frame.sample_rate,
                    frame.channels,
                    this_us as f64 / 1e6,
                    produced
                );
            }
            if produced == 0 {
                continue;
            }
            pcm::to_stereo(&out[..produced], (*channels).clamp(1, 2), &mut stereo);
            let source: &[i16] = if resampler.passthrough() {
                &stereo
            } else {
                resampled.clear();
                resampler.process(&stereo, &mut resampled);
                &resampled
            };
            // Other speeds than 1x: stretch the sound without changing its pitch.
            let speed = pipe.speed();
            let samples: &[i16] = if (speed - 1.0).abs() < 1e-3 {
                stretching = false;
                source
            } else {
                if !stretching {
                    stretching = true;
                    stretcher.reset();
                }
                stretcher.set_speed(speed);
                stretched.clear();
                stretcher.process(source, &mut stretched);
                &stretched
            };
            base_us.get_or_insert(this_us.max(pipe.start_us));
            let mut done = 0;
            while done < samples.len() {
                if pipe.stopped() {
                    break 'outer;
                }
                renderer.set_volume(f32::from_bits(pipe.volume.load(Ordering::Acquire)));
                if pipe.paused.load(Ordering::Acquire) {
                    publish_clock(&pipe, &renderer, base_us, pushed, &mut ledger);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let accepted = renderer.push(&samples[done..]);
                if accepted == 0 {
                    publish_clock(&pipe, &renderer, base_us, pushed, &mut ledger);
                    std::thread::sleep(Duration::from_millis(5));
                } else {
                    done += accepted * 2;
                    pushed += accepted as u64;
                    ledger.push(accepted as u64, if stretching { speed } else { 1.0 });
                }
            }
            publish_clock(&pipe, &renderer, base_us, pushed, &mut ledger);
        }
    }
    // Everything is queued: let it play out so the end of the video is not cut short. The device
    // keeps a little silence queued after the last sample, so "done" means "almost empty".
    let almost_empty = (renderer.period_frames() as usize * 2).max(2048);
    let give_up = Instant::now() + Duration::from_millis(500) + Duration::from_micros(renderer.buffered_frames() as u64 * 1_000_000 / u64::from(pcm::OUT_RATE));
    while !pipe.stopped() && renderer.buffered_frames() > almost_empty && Instant::now() < give_up {
        publish_clock(&pipe, &renderer, base_us, pushed, &mut ledger);
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(base) = base_us {
        pipe.audio_clock_us.store(base + ledger.media_us(pushed) as i64, Ordering::Release);
    }
    pipe.audio_done.store(true, Ordering::Release);
}

fn setup_thread(url: String, shared: Arc<Shared>) {
    let fetch = |u: &str| -> std::result::Result<String, String> {
        http::get(u).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| format!("{u}: {e}"))
    };
    let outcome = (|| -> std::result::Result<Setup, String> {
        let text = fetch(&url)?;
        if !text.trim_start().starts_with("#EXTM3U") {
            return Err("la dirección no devuelve una lista HLS".into());
        }
        let mut ladder = Vec::new();
        let mut subtitles = Vec::new();
        let (video_url, audio_url, has_audio, initial) = if text.contains("#EXT-X-STREAM-INF") {
            let master = playlist::parse_master(&text, &url);
            ladder = playlist::ladder(&master, MAX_HEIGHT).into_iter().cloned().collect();
            if ladder.is_empty() {
                return Err("la lista no ofrece ninguna calidad H.264 reproducible".into());
            }
            // Starts with the best quality; the first downloads tell whether the link keeps up.
            let initial = ladder.len() - 1;
            let variant = &ladder[initial];
            tracing::info!(
                "HLS: {} calidades reproducibles ({}); empieza con {}x{} ({} kbps, {}), {} variantes en la lista",
                ladder.len(),
                ladder.iter().map(|v| format!("{}p", v.height)).collect::<Vec<_>>().join(", "),
                variant.width,
                variant.height,
                variant.bandwidth / 1000,
                variant.codecs,
                master.variants.len()
            );
            let audio = playlist::audio_for(&master, variant).map(str::to_owned);
            let has_audio = audio.is_some() || variant.codecs.to_ascii_lowercase().contains("mp4a");
            subtitles = master
                .subtitles
                .iter()
                .filter(|t| variant.subtitles.as_deref().is_none_or(|g| g == t.group))
                .cloned()
                .collect();
            (variant.uri.clone(), audio, has_audio, initial)
        } else {
            (url.clone(), None, true, 0)
        };
        let video = playlist::parse_media(&fetch(&video_url)?, &video_url);
        if video.segments.is_empty() {
            return Err("la lista de vídeo no tiene fragmentos".into());
        }
        let audio = match &audio_url {
            Some(u) => Some(Arc::new(playlist::parse_media(&fetch(u)?, u))),
            None => None,
        };
        tracing::info!(
            "HLS: {} fragmentos de vídeo, {}{}{}{}",
            video.segments.len(),
            if video.ended { format!("{:.0} s", video.duration()) } else { "en directo".to_owned() },
            if audio.is_some() { ", con audio aparte" } else { "" },
            if video.map.is_some() { ", fMP4" } else { "" },
            if subtitles.is_empty() { String::new() } else { format!(", {} pistas de subtítulos", subtitles.len()) }
        );
        let rungs = ladder.iter().map(|v| Rung { bandwidth: v.bandwidth, height: v.height }).collect();
        let video = Arc::new(video);
        // The audio time line starts where the same segment of the video would.
        let audio_start = match &audio {
            Some(a) if !video.ended => (a.first_sequence as f64 - video.first_sequence as f64) * video.target(),
            _ => 0.0,
        };
        if let Ok(mut timelines) = shared.timelines.lock() {
            timelines.video = Timeline::new(&video, 0.0);
            timelines.audio = audio.as_ref().map(|a| Timeline::new(a, audio_start));
        }
        if let Some(top) = ladder.last().filter(|v| v.width > 0 && v.height > 0) {
            shared.display.store(pack_size(top.width, top.height), Ordering::Release);
        }
        shared.rung.store(initial, Ordering::Release);
        Ok(Setup { ladder, rungs, initial, video_url, video, audio_url, audio, has_audio, subtitles })
    })();
    match outcome {
        Ok(setup) => {
            if let Ok(mut slot) = shared.setup.lock() {
                *slot = Some(Arc::new(setup));
            }
        }
        Err(e) => shared.failure.set(e),
    }
}

struct Shared {
    setup: Mutex<Option<Arc<Setup>>>,
    failure: Arc<Failure>,
    // Size of the latest decoded picture.
    width: AtomicU32,
    height: AtomicU32,
    // Size the window is fitted to (width << 32 | height), 0 until known.
    display: AtomicU64,
    // Seek request in milliseconds, or -1.
    seek_ms: AtomicI64,
    // Linear volume as `f32` bits; the audio thread applies it to the renderer.
    volume: Arc<AtomicU32>,
    speed: Arc<AtomicU32>,
    // -1 for automatic quality, else the ladder index.
    quality: AtomicI32,
    // The ladder index being downloaded.
    rung: AtomicUsize,
    timelines: Mutex<Timelines>,
    // Finished playlists by address.
    playlists: Mutex<HashMap<String, Arc<Media>>>,
    // A live playlist reached its end.
    ended: AtomicBool,
    // Where the video's time stamps sit on the media time line (for subtitles).
    origin: vtt::Origin,
}

impl Shared {
    fn quality(&self) -> Quality {
        match self.quality.load(Ordering::Acquire) {
            i if i >= 0 => Quality::Fixed(i as usize),
            _ => Quality::Auto,
        }
    }
}

// One entry of the quality menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QualityLevel {
    pub height: u32,
    pub bandwidth: u64,
}

struct Subtitles {
    selected: Option<usize>,
    cues: Arc<vtt::Cues>,
    stop: Arc<AtomicBool>,
}

pub(super) struct Native {
    shared: Arc<Shared>,
    gpu: Arc<GpuDevice>,
    pipe: Option<Arc<Pipe>>,
    blitter: Option<blit::Blitter>,
    current: Option<DecodedFrame>,
    paused: bool,
    // Where the next pipeline starts, in seconds.
    start: f64,
    ended: bool,
    live_start_done: bool,
    rate: f64,
    subtitles: Subtitles,
}

impl Native {
    pub(super) fn open(url: &str, gpu: &Arc<GpuDevice>) -> Result<Self> {
        let shared = Arc::new(Shared {
            setup: Mutex::new(None),
            failure: Arc::new(Failure::default()),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            display: AtomicU64::new(0),
            seek_ms: AtomicI64::new(-1),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            speed: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            quality: AtomicI32::new(-1),
            rung: AtomicUsize::new(0),
            timelines: Mutex::new(Timelines { video: Timeline::default(), audio: None }),
            playlists: Mutex::new(HashMap::new()),
            ended: AtomicBool::new(false),
            origin: Arc::new(Mutex::new(None)),
        });
        let worker = Arc::clone(&shared);
        let url = url.to_owned();
        std::thread::Builder::new()
            .name("rustplay-hls-setup".into())
            .spawn(move || setup_thread(url, worker))
            .map_err(|e| crate::Error::Media(format!("no se pudo crear el hilo del reproductor: {e}")))?;
        Ok(Self {
            shared,
            gpu: Arc::clone(gpu),
            pipe: None,
            blitter: None,
            current: None,
            paused: false,
            start: 0.0,
            ended: false,
            live_start_done: false,
            rate: 1.0,
            subtitles: Subtitles { selected: None, cues: Arc::default(), stop: Arc::new(AtomicBool::new(false)) },
        })
    }

    fn setup(&self) -> Option<Arc<Setup>> {
        self.shared.setup.lock().ok().and_then(|s| s.clone())
    }

    fn start_pipeline(&mut self, setup: &Arc<Setup>, at: f64) {
        if let Some(old) = self.pipe.take() {
            old.stop.store(true, Ordering::Release);
            old.space.notify_all();
        }
        self.current = None;
        self.ended = false;
        let start_us = (at * 1e6) as i64;
        let (video_sequence, audio_sequence) = match self.shared.timelines.lock() {
            Ok(t) => (t.video.locate(at).0, t.audio.as_ref().map(|a| a.locate(at).0)),
            Err(_) => return,
        };
        let rung = match self.shared.quality() {
            Quality::Fixed(i) => i.min(setup.ladder.len().saturating_sub(1)),
            Quality::Auto => self.shared.rung.load(Ordering::Acquire).min(setup.ladder.len().saturating_sub(1)),
        };
        let pipe = Arc::new(Pipe {
            created: Instant::now(),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(self.paused),
            start_us,
            frames: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
            video_done: AtomicBool::new(false),
            audio_done: AtomicBool::new(!setup.has_audio),
            has_audio: AtomicBool::new(setup.has_audio),
            audio_clock_us: AtomicI64::new(NOT_STARTED),
            wall: Mutex::new(Wall::default()),
            size: AtomicU32::new(0),
            failure: Arc::clone(&self.shared.failure),
            volume: Arc::clone(&self.shared.volume),
            speed: Arc::clone(&self.shared.speed),
        });
        let (vtx, vrx) = std::sync::mpsc::sync_channel::<Item>(QUEUED_VIDEO_PACKETS);
        let (atx, arx) = std::sync::mpsc::sync_channel::<Item>(QUEUED_AUDIO_PACKETS);
        let separate_audio = setup.audio.is_some();
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
                self.shared.failure.set(format!("no se pudo crear un hilo del reproductor: {e}"));
            }
        };
        let (p, shared) = (Arc::clone(&pipe), Arc::clone(&self.shared));
        let gpu = Arc::clone(&self.gpu);
        spawn("rustplay-hls-video", Box::new(move || video_thread(p, gpu, vrx, shared)));
        if setup.has_audio {
            let p = Arc::clone(&pipe);
            spawn("rustplay-hls-audio", Box::new(move || audio_thread(p, arx)));
        }
        let feeder = Feeder {
            pipe: Arc::clone(&pipe),
            shared: Arc::clone(&self.shared),
            setup: Arc::clone(setup),
            rung,
            first_sequence: video_sequence,
            video: Some(vtx),
            audio: (!separate_audio && setup.has_audio).then(|| atx.clone()),
        };
        spawn("rustplay-hls-feed-video", Box::new(move || feeder.run()));
        if separate_audio {
            let feeder = Feeder {
                pipe: Arc::clone(&pipe),
                shared: Arc::clone(&self.shared),
                setup: Arc::clone(setup),
                rung: 0,
                first_sequence: audio_sequence.unwrap_or(0),
                video: None,
                audio: Some(atx),
            };
            spawn("rustplay-hls-feed-audio", Box::new(move || feeder.run()));
        }
        pipe.set_speed(self.rate as f32);
        self.pipe = Some(pipe);
    }

    pub(super) fn poll(&mut self) {
        let Some(setup) = self.setup() else { return };
        if !self.live_start_done {
            self.live_start_done = true;
            // A live stream starts a few segments behind its newest one.
            if !setup.video.ended
                && let Ok(t) = self.shared.timelines.lock()
            {
                self.start = (t.video.end() - LIVE_EDGE_SEGMENTS * setup.video.target()).max(t.video.window_start());
            }
        }
        let request = self.shared.seek_ms.swap(-1, Ordering::AcqRel);
        if request >= 0 {
            let (low, high) = self.shared.timelines.lock().map_or((0.0, f64::MAX), |t| (t.video.window_start(), t.video.end()));
            self.start = (request as f64 / 1000.0).clamp(low, high.max(low));
            self.start_pipeline(&setup, self.start);
        } else if self.pipe.is_none() {
            self.start_pipeline(&setup, self.start);
        }
    }

    pub(super) fn opened(&self) -> bool {
        self.shared.width.load(Ordering::Acquire) > 0
    }

    pub(super) fn ended(&self) -> bool {
        self.ended
    }

    pub(super) fn failed(&self) -> bool {
        self.shared.failure.get().is_some()
    }

    pub(super) fn error(&self) -> String {
        self.shared.failure.get().unwrap_or_default()
    }

    pub(super) fn paused(&self) -> bool {
        self.paused || self.ended
    }

    pub(super) fn buffering(&self) -> bool {
        match &self.pipe {
            None => !self.failed(),
            Some(pipe) => !self.ended && !self.paused && pipe.frames.lock().is_ok_and(|f| f.is_empty()) && !pipe.video_done.load(Ordering::Acquire),
        }
    }

    // Whether the stream has no end yet (a live playlist that has not been closed).
    fn live(&self) -> bool {
        self.setup().is_some_and(|s| !s.video.ended) && !self.shared.ended.load(Ordering::Acquire)
    }

    pub(super) fn duration(&self) -> f64 {
        if self.setup().is_none() || self.live() {
            return f64::INFINITY;
        }
        self.shared.timelines.lock().map_or(f64::INFINITY, |t| t.video.end())
    }

    // The part of a live stream that can still be reached, in seconds on the media time line.
    pub(super) fn live_window(&self) -> Option<(f64, f64)> {
        if !self.live() {
            return None;
        }
        self.shared.timelines.lock().ok().map(|t| (t.video.window_start(), t.video.end()))
    }

    pub(super) fn position(&self) -> f64 {
        self.pipe.as_ref().map_or(self.start, |p| p.now_us() as f64 / 1e6).max(0.0)
    }

    // The size the window should fit: it does not change when the quality does.
    pub(super) fn video_size(&self) -> Option<(u32, u32)> {
        if self.shared.width.load(Ordering::Acquire) == 0 {
            return None;
        }
        let d = self.shared.display.load(Ordering::Acquire);
        let (w, h) = ((d >> 32) as u32, d as u32);
        (w > 0 && h > 0).then_some((w, h))
    }

    pub(super) fn play(&mut self) {
        if self.ended {
            self.start = 0.0;
            self.paused = false;
            self.shared.seek_ms.store(0, Ordering::Release);
            return;
        }
        self.paused = false;
        if let Some(pipe) = &self.pipe {
            pipe.set_paused(false);
        }
    }

    pub(super) fn pause(&self) {
        // The state flag lives in the pipeline; `paused` here follows it on the next frame.
        if let Some(pipe) = &self.pipe {
            pipe.set_paused(true);
        }
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Release);
    }

    pub(super) fn seek(&self, seconds: f64) {
        self.shared.seek_ms.store((seconds.max(0.0) * 1000.0) as i64, Ordering::Release);
    }

    // Playback speed: the sound is stretched and the clock scaled, the picture follows the clock.
    pub(super) fn set_rate(&mut self, rate: f64) {
        self.rate = rate.clamp(stretch::MIN_SPEED, stretch::MAX_SPEED);
        self.shared.speed.store((self.rate as f32).to_bits(), Ordering::Release);
        if let Some(pipe) = &self.pipe {
            pipe.set_speed(self.rate as f32);
        }
    }

    // The qualities of the stream, worst to best; empty when there is only one.
    pub(super) fn quality_levels(&self) -> Vec<QualityLevel> {
        match self.setup() {
            Some(s) if s.ladder.len() > 1 => s.ladder.iter().map(|v| QualityLevel { height: v.height, bandwidth: v.bandwidth }).collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn quality(&self) -> Quality {
        self.shared.quality()
    }

    // The ladder index being downloaded now.
    pub(super) fn playing_rung(&self) -> usize {
        self.shared.rung.load(Ordering::Acquire)
    }

    // "Automatic" adapts at segment boundaries; a fixed quality restarts the pipeline at the
    // current position so the change is seen at once.
    pub(super) fn set_quality(&self, quality: Quality) {
        let value = match quality {
            Quality::Auto => -1,
            Quality::Fixed(i) => i as i32,
        };
        if self.shared.quality.swap(value, Ordering::AcqRel) == value {
            return;
        }
        if let Quality::Fixed(i) = quality
            && i != self.playing_rung()
        {
            self.seek(self.position());
        }
    }

    pub(super) fn subtitle_names(&self) -> Vec<String> {
        self.setup().map(|s| s.subtitles.iter().map(|t| t.name.clone()).collect()).unwrap_or_default()
    }

    pub(super) fn subtitle_count(&self) -> usize {
        self.setup().map_or(0, |s| s.subtitles.len())
    }

    pub(super) fn subtitle(&self) -> Option<usize> {
        self.subtitles.selected
    }

    pub(super) fn set_subtitle(&mut self, which: Option<usize>) {
        if which == self.subtitles.selected {
            return;
        }
        self.subtitles.stop.store(true, Ordering::Release);
        self.subtitles = Subtitles { selected: None, cues: Arc::default(), stop: Arc::new(AtomicBool::new(false)) };
        let Some(track) = which.and_then(|i| self.setup().and_then(|s| s.subtitles.get(i).cloned())) else { return };
        self.subtitles.selected = which;
        let (cues, origin, stop) = (Arc::clone(&self.subtitles.cues), Arc::clone(&self.shared.origin), Arc::clone(&self.subtitles.stop));
        if let Err(e) = std::thread::Builder::new().name("rustplay-hls-subtitles".into()).spawn(move || vtt::load(track, cues, origin, stop)) {
            tracing::warn!("no se pudo crear el hilo de subtítulos: {e}");
            self.subtitles.selected = None;
        }
    }

    // The subtitle text to show now.
    pub(super) fn cue_text(&self) -> Option<String> {
        self.subtitles.selected?;
        self.subtitles.cues.at(self.position())
    }

    // Shows the picture that is due now inside `dst` of `texture`.
    pub(super) fn copy_frame(&mut self, texture: &ID3D11Texture2D, dst: (f32, f32, f32, f32), size: (u32, u32), force: bool) -> bool {
        let Some(pipe) = self.pipe.clone() else { return false };
        self.paused = pipe.paused.load(Ordering::Acquire);
        // Sound that never starts (a stalled or missing audio stream) must not freeze the picture.
        if pipe.has_audio.load(Ordering::Acquire)
            && pipe.audio_clock_us.load(Ordering::Acquire) == NOT_STARTED
            && pipe.created.elapsed() > AUDIO_PATIENCE
            && pipe.frames.lock().is_ok_and(|f| !f.is_empty())
        {
            tracing::warn!("el audio no ha llegado a empezar: se reproduce solo la imagen");
            pipe.has_audio.store(false, Ordering::Release);
            pipe.audio_done.store(true, Ordering::Release);
        }
        let now = pipe.now_us();
        let mut changed = false;
        if let Ok(mut frames) = pipe.frames.lock() {
            while frames.front().is_some_and(|f| f.time_us <= now + 15_000) {
                if let Some(next) = frames.pop_front() {
                    pipe.start_wall();
                    self.current = Some(next.frame);
                    changed = true;
                }
            }
            if changed {
                pipe.space.notify_all();
            }
            // No audio clock, nothing left and nothing coming: the video has finished.
            let finished = pipe.video_done.load(Ordering::Acquire)
                && frames.is_empty()
                && (pipe.audio_done.load(Ordering::Acquire) || !pipe.has_audio.load(Ordering::Acquire));
            if finished && !self.ended && !pipe.stopped() && self.shared.failure.get().is_none() {
                self.ended = true;
            }
        }
        if !(changed || force) {
            return false;
        }
        let Some(frame) = &self.current else { return false };
        if self.blitter.is_none() {
            match blit::Blitter::new(&self.gpu) {
                Ok(b) => self.blitter = Some(b),
                Err(e) => {
                    self.shared.failure.set(format!("no se pudo preparar la conversión de imagen: {e}"));
                    return false;
                }
            }
        }
        match self.blitter.as_mut().map(|b| b.blit(frame, texture, size, dst)) {
            Some(Ok(())) => true,
            Some(Err(e)) => {
                tracing::debug!("no se pudo dibujar el fotograma: {e}");
                false
            }
            None => false,
        }
    }

    pub(super) fn close(&mut self) {
        if let Some(pipe) = self.pipe.take() {
            pipe.stop.store(true, Ordering::Release);
            pipe.space.notify_all();
        }
        self.subtitles.stop.store(true, Ordering::Release);
        self.current = None;
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    use super::*;
    use crate::media::video::{EncoderConfig, H264Encoder};

    fn nv12(gpu: &GpuDevice, w: u32, h: u32, frame: u32) -> Option<ID3D11Texture2D> {
        let mut pixels = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                pixels[(y * w + x) as usize] = ((x + y + frame * 8) % 220 + 16) as u8;
            }
        }
        for (i, c) in pixels[(w * h) as usize..].iter_mut().enumerate() {
            *c = if i % 2 == 0 { 100 + (frame % 50) as u8 } else { 150 };
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w, SysMemSlicePitch: 0 };
        let mut tex = None;
        // SAFETY: datos iniciales válidos durante la llamada.
        unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).ok()? };
        tex
    }

    // Serves a playlist and its segments from memory on loopback; returns the playlist address.
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let files = files.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                    let status = if body.is_some() { "200 OK" } else { "404 Not Found" };
                    let body = body.unwrap_or_default();
                    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(&body);
                });
            }
        });
        format!("http://127.0.0.1:{port}/list.m3u8")
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn plays_a_two_segment_stream_to_the_end() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (320u32, 240u32);
        let mut encoder =
            H264Encoder::new(Arc::clone(&gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 }).expect("codificador");
        let textures: Vec<_> = (0..4).filter_map(|i| nv12(&gpu, w, h, i)).collect();
        assert_eq!(textures.len(), 4);
        let mut units = Vec::new();
        for i in 0..60u32 {
            let pts = i64::from(i) * 10_000_000 / 30;
            if let Some(frame) = encoder.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                units.push(frame.data.to_vec());
            }
            while let Some(frame) = encoder.poll_output().expect("salida") {
                units.push(frame.data.to_vec());
            }
        }
        assert!(units.len() >= 50, "pocas unidades: {}", units.len());
        // Two one-second segments; times start at 10 s, as real streams do.
        let base = 900_000u64;
        let half = units.len() / 2;
        let seg = |range: std::ops::Range<usize>| {
            let video: Vec<(u64, Vec<u8>)> = range.map(|i| (base + i as u64 * 3000, units[i].clone())).collect();
            ts::tests::segment(&video, &[])
        };
        let list = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\ns0.ts\n#EXTINF:1.0,\ns1.ts\n#EXT-X-ENDLIST\n";
        let url = serve(vec![
            ("/list.m3u8".into(), list.as_bytes().to_vec()),
            ("/s0.ts".into(), seg(0..half)),
            ("/s1.ts".into(), seg(half..units.len())),
        ]);

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut target = None;
        // SAFETY: descripción válida.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut target)).expect("textura") };
        let target = target.expect("textura");

        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let (mut shown, mut max_position) = (0u32, 0.0f64);
        while started.elapsed() < Duration::from_secs(12) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            if native.opened() && native.copy_frame(&target, (0.0, 0.0, w as f32, h as f32), (w, h), false) {
                shown += 1;
            }
            max_position = max_position.max(native.position());
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended(), "no terminó (imágenes mostradas: {shown}, posición {max_position:.2})");
        assert_eq!(native.video_size(), Some((w, h)));
        assert!((native.duration() - 2.0).abs() < 0.01);
        assert!(shown >= 30, "pocas imágenes: {shown}");
        assert!(max_position > 1.5, "el reloj no avanzó: {max_position}");
        let image = crate::media::screenshot::read_texture(&gpu, &target).expect("leer");
        let lit = image.pixels.chunks_exact(4).filter(|p| p[0] > 20 || p[1] > 20 || p[2] > 20).count();
        assert!(lit > 1000, "la imagen está vacía ({lit} píxeles)");
        native.close();
    }

    // Raw AAC frames of a quiet tone, made with the Windows encoder, as 44.1 kHz stereo ADTS.
    fn adts_frames(seconds: usize) -> Vec<Vec<u8>> {
        use windows::Win32::Media::MediaFoundation::*;
        let xf = crate::media::mf::create_transform(&crate::media::mf::CLSID_AAC_ENCODER).expect("codificador AAC");
        // SAFETY: negociación de tipos en una MFT de prueba.
        unsafe {
            let t = MFCreateMediaType().unwrap();
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).unwrap();
            t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).unwrap();
            t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 44_100).unwrap();
            t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).unwrap();
            t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20_000).unwrap();
            t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).unwrap();
            t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29).unwrap();
            xf.SetOutputType(0, &t, 0).unwrap();
            xf.SetInputType(0, &crate::media::mf::pcm_type(44_100, 2).unwrap(), 0).unwrap();
        }
        let pcm = crate::media::audio::tests::sine(44_100 / 1024 * 1024 * seconds, 0, 600.0);
        crate::media::audio::tests::run_encoder(xf, &pcm, 1024)
            .into_iter()
            .map(|raw| {
                let len = raw.len() + 7;
                let mut frame = vec![0xFF, 0xF1, 0x50, 0x80 | ((len >> 11) & 3) as u8, (len >> 3) as u8, (((len & 7) << 5) | 0x1F) as u8, 0xFC];
                frame.extend_from_slice(&raw);
                frame
            })
            .collect()
    }

    #[test]
    #[ignore = "needs a GPU, an H.264 encoder and an audio device (plays a very quiet tone)"]
    fn plays_video_with_sound_and_follows_the_audio_clock() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (320u32, 240u32);
        let mut encoder =
            H264Encoder::new(Arc::clone(&gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 }).expect("codificador");
        let textures: Vec<_> = (0..4).filter_map(|i| nv12(&gpu, w, h, i)).collect();
        let mut units = Vec::new();
        for i in 0..60u32 {
            let pts = i64::from(i) * 10_000_000 / 30;
            if let Some(frame) = encoder.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                units.push(frame.data.to_vec());
            }
            while let Some(frame) = encoder.poll_output().expect("salida") {
                units.push(frame.data.to_vec());
            }
        }
        let audio = adts_frames(2);
        assert!(audio.len() >= 60, "pocos paquetes de audio: {}", audio.len());
        let base = 900_000u64;
        // One PES per 8 ADTS frames, timed from the first of them.
        let audio_pes = |range: std::ops::Range<usize>| -> Vec<(u64, Vec<u8>)> {
            range
                .step_by(8)
                .map(|i| {
                    let end = (i + 8).min(audio.len());
                    (base + i as u64 * 1024 * 90_000 / 44_100, audio[i..end].concat())
                })
                .collect()
        };
        let half = units.len() / 2;
        let audio_half = audio.len() / 2 / 8 * 8;
        let video = |range: std::ops::Range<usize>| -> Vec<(u64, Vec<u8>)> {
            range.map(|i| (base + i as u64 * 3000, units[i].clone())).collect()
        };
        let list = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\ns0.ts\n#EXTINF:1.0,\ns1.ts\n#EXT-X-ENDLIST\n";
        let url = serve(vec![
            ("/list.m3u8".into(), list.as_bytes().to_vec()),
            ("/s0.ts".into(), ts::tests::segment(&video(0..half), &audio_pes(0..audio_half))),
            ("/s1.ts".into(), ts::tests::segment(&video(half..units.len()), &audio_pes(audio_half..audio.len()))),
        ]);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut target = None;
        // SAFETY: descripción válida.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut target)).expect("textura") };
        let target = target.expect("textura");

        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let (mut shown, mut max_position) = (0u32, 0.0f64);
        let mut used_audio_clock = false;
        while started.elapsed() < Duration::from_secs(12) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            if native.opened() && native.copy_frame(&target, (0.0, 0.0, w as f32, h as f32), (w, h), false) {
                shown += 1;
            }
            if let Some(p) = &native.pipe {
                used_audio_clock |= p.has_audio.load(Ordering::Acquire) && p.audio_clock_us.load(Ordering::Acquire) != NOT_STARTED;
            }
            max_position = max_position.max(native.position());
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended(), "no terminó (imágenes: {shown}, posición {max_position:.2})");
        assert!(used_audio_clock, "el reloj de audio nunca empezó");
        assert!(shown >= 30, "pocas imágenes: {shown}");
        assert!(max_position > 1.4, "el reloj no avanzó: {max_position}");
        native.close();
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn seeking_restarts_from_the_right_segment() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (320u32, 240u32);
        let mut encoder =
            H264Encoder::new(Arc::clone(&gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 }).expect("codificador");
        let textures: Vec<_> = (0..4).filter_map(|i| nv12(&gpu, w, h, i)).collect();
        let mut units = Vec::new();
        // Every 30th frame is a keyframe so each segment starts with an IDR, as in real HLS.
        for i in 0..90u32 {
            if i % 30 == 0 {
                encoder.request_keyframe();
            }
            let pts = i64::from(i) * 10_000_000 / 30;
            if let Some(frame) = encoder.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                units.push(frame.data.to_vec());
            }
            while let Some(frame) = encoder.poll_output().expect("salida") {
                units.push(frame.data.to_vec());
            }
        }
        assert!(units.len() >= 85, "pocas unidades: {}", units.len());
        let third = units.len() / 3;
        let base = 900_000u64;
        let seg = |range: std::ops::Range<usize>| {
            let video: Vec<(u64, Vec<u8>)> = range.map(|i| (base + i as u64 * 3000, units[i].clone())).collect();
            ts::tests::segment(&video, &[])
        };
        let list = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\ns0.ts\n#EXTINF:1.0,\ns1.ts\n#EXTINF:1.0,\ns2.ts\n#EXT-X-ENDLIST\n";
        let url = serve(vec![
            ("/list.m3u8".into(), list.as_bytes().to_vec()),
            ("/s0.ts".into(), seg(0..third)),
            ("/s1.ts".into(), seg(third..2 * third)),
            ("/s2.ts".into(), seg(2 * third..units.len())),
        ]);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut target = None;
        // SAFETY: descripción válida.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut target)).expect("textura") };
        let target = target.expect("textura");

        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let mut sought = false;
        let mut first_after_seek = None;
        let mut shown_after = 0u32;
        while started.elapsed() < Duration::from_secs(12) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            let shown = native.opened() && native.copy_frame(&target, (0.0, 0.0, w as f32, h as f32), (w, h), false);
            if !sought && shown {
                native.seek(2.0);
                sought = true;
            } else if sought && shown {
                first_after_seek.get_or_insert(native.position());
                shown_after += 1;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(sought, "nunca llegó una imagen");
        assert!(native.ended(), "no terminó tras el salto");
        let first = first_after_seek.expect("sin imágenes tras el salto");
        assert!(first >= 1.9, "tras saltar a 2 s la posición era {first:.2}");
        assert!((20..=40).contains(&shown_after), "se esperaban unas 30 imágenes del último segundo, hubo {shown_after}");
        native.close();
    }

    // ---- Adaptive quality, live playlists and fragmented MP4 ----------------------------------

    fn encode_units(gpu: &Arc<GpuDevice>, w: u32, h: u32, frames: u32) -> Vec<Vec<u8>> {
        let mut encoder =
            H264Encoder::new(Arc::clone(gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 }).expect("codificador");
        let textures: Vec<_> = (0..4).filter_map(|i| nv12(gpu, w, h, i)).collect();
        assert_eq!(textures.len(), 4);
        let mut units = Vec::new();
        for i in 0..frames {
            if i % 30 == 0 {
                encoder.request_keyframe();
            }
            let pts = i64::from(i) * 10_000_000 / 30;
            if let Some(frame) = encoder.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                units.push(frame.data.to_vec());
            }
            while let Some(frame) = encoder.poll_output().expect("salida") {
                units.push(frame.data.to_vec());
            }
        }
        units
    }

    // NAL units of an Annex B access unit, without start codes.
    fn nal_units(unit: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= unit.len() {
            if unit[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .map(|(n, &s)| {
                let mut end = starts.get(n + 1).map_or(unit.len(), |&next| next - 3);
                while end > s && unit[end - 1] == 0 {
                    end -= 1;
                }
                &unit[s..end]
            })
            .collect()
    }

    fn is_key(unit: &[u8]) -> bool {
        nal_units(unit).iter().any(|n| n.first().is_some_and(|b| b & 0x1F == 5))
    }

    // Index ranges of the units, one per group that starts with a keyframe.
    fn key_ranges(units: &[Vec<u8>]) -> Vec<std::ops::Range<usize>> {
        let keys: Vec<usize> = (0..units.len()).filter(|&i| is_key(&units[i])).collect();
        keys.iter().enumerate().map(|(n, &s)| s..keys.get(n + 1).copied().unwrap_or(units.len())).collect()
    }

    fn target_texture(gpu: &GpuDevice, w: u32, h: u32) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut target = None;
        // SAFETY: descripción válida.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut target)).expect("textura") };
        target.expect("textura")
    }

    // Like `serve`, but the answer comes from a function (so playlists can change over time).
    fn serve_fn(handler: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static) -> String {
        let handler = Arc::new(handler);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let body = handler(&path);
                    let status = if body.is_some() { "200 OK" } else { "404 Not Found" };
                    let body = body.unwrap_or_default();
                    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(&body);
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn ts_segments(units: &[Vec<u8>], ranges: &[std::ops::Range<usize>]) -> Vec<Vec<u8>> {
        let base = 900_000u64;
        ranges
            .iter()
            .map(|r| {
                let video: Vec<(u64, Vec<u8>)> = r.clone().map(|i| (base + i as u64 * 3000, units[i].clone())).collect();
                ts::tests::segment(&video, &[])
            })
            .collect()
    }

    fn playlist_of(count: usize, prefix: &str, extension: &str, ended: bool) -> String {
        let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n");
        for i in 0..count {
            text += &format!("#EXTINF:1.0,\n{prefix}{i}.{extension}\n");
        }
        if ended {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    }

    // A master with a 160x120 and a 320x240 variant; the segments of the big one answer after
    // `delay`. Returns the address of the master playlist.
    fn two_quality_stream(gpu: &Arc<GpuDevice>, delay: Duration) -> String {
        let low = encode_units(gpu, 160, 120, 120);
        let high = encode_units(gpu, 320, 240, 120);
        let (low_ranges, high_ranges) = (key_ranges(&low), key_ranges(&high));
        let count = low_ranges.len().min(high_ranges.len());
        assert!(count >= 3, "pocos grupos de imágenes: {count}");
        let low_segments = ts_segments(&low, &low_ranges[..count]);
        let high_segments = ts_segments(&high, &high_ranges[..count]);
        let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=100000,CODECS=\"avc1.42C01E\",RESOLUTION=160x120\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.42C01E\",RESOLUTION=320x240\nhigh.m3u8\n";
        let (low_list, high_list) = (playlist_of(count, "l", "ts", true), playlist_of(count, "h", "ts", true));
        let base = serve_fn(move |path| {
            match path {
                "/master.m3u8" => return Some(master.as_bytes().to_vec()),
                "/low.m3u8" => return Some(low_list.as_bytes().to_vec()),
                "/high.m3u8" => return Some(high_list.as_bytes().to_vec()),
                _ => {}
            }
            let index: usize = path.trim_start_matches("/l").trim_start_matches("/h").trim_end_matches(".ts").parse().ok()?;
            if path.starts_with("/l") {
                low_segments.get(index).cloned()
            } else {
                std::thread::sleep(delay);
                high_segments.get(index).cloned()
            }
        });
        format!("{base}/master.m3u8")
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn drops_to_a_lower_quality_on_a_slow_link_without_resizing_the_window() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let url = two_quality_stream(&gpu, Duration::from_millis(800));
        let target = target_texture(&gpu, 320, 240);
        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let mut sizes = std::collections::BTreeSet::new();
        while started.elapsed() < Duration::from_secs(20) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            native.copy_frame(&target, (0.0, 0.0, 320.0, 240.0), (320, 240), false);
            if let Some(f) = &native.current {
                sizes.insert((f.width, f.height));
            }
            if native.opened() {
                assert_eq!(native.video_size(), Some((320, 240)), "the window size must not follow the quality");
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended(), "no terminó; tamaños vistos: {sizes:?}");
        assert!(sizes.contains(&(320, 240)), "empieza con la mejor calidad: {sizes:?}");
        assert!(sizes.contains(&(160, 120)), "no bajó de calidad: {sizes:?}");
        assert_eq!(native.playing_rung(), 0);
        assert_eq!(native.quality_levels().len(), 2);
        let image = crate::media::screenshot::read_texture(&gpu, &target).expect("leer");
        let lit = image.pixels.chunks_exact(4).filter(|p| p[0] > 20 || p[1] > 20 || p[2] > 20).count();
        assert!(lit > 1000, "la imagen está vacía ({lit} píxeles)");
        native.close();
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn a_fixed_quality_applies_at_once_and_a_fast_link_stays_on_top() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let url = two_quality_stream(&gpu, Duration::ZERO);
        let target = target_texture(&gpu, 320, 240);
        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let mut sizes = Vec::new();
        let mut chose = false;
        while started.elapsed() < Duration::from_secs(20) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            let shown = native.copy_frame(&target, (0.0, 0.0, 320.0, 240.0), (320, 240), false);
            if shown && let Some(f) = &native.current {
                if sizes.last() != Some(&(f.width, f.height)) {
                    sizes.push((f.width, f.height));
                }
                if !chose {
                    chose = true;
                    native.set_quality(Quality::Fixed(0));
                    assert_eq!(native.quality(), Quality::Fixed(0));
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended());
        // Top quality first (a fast link never steps down), then the chosen one for the rest.
        assert_eq!(sizes, [(320, 240), (160, 120)]);
        native.close();
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn follows_a_live_playlist_until_it_ends() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let units = encode_units(&gpu, 320, 240, 120);
        let ranges = key_ranges(&units);
        assert!(ranges.len() >= 4, "pocos grupos de imágenes: {}", ranges.len());
        let segments = ts_segments(&units, &ranges[..4]);
        let clock = Instant::now();
        // A two segment window that slides every second; the playlist is closed after three.
        let base = serve_fn(move |path| {
            if path == "/live.m3u8" {
                let step = (clock.elapsed().as_secs_f64() as usize).min(2);
                let mut text = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{step}\n");
                for i in step..step + 2 {
                    text += &format!("#EXTINF:1.0,\ns{i}.ts\n");
                }
                if clock.elapsed() > Duration::from_millis(3200) {
                    text += "#EXT-X-ENDLIST\n";
                }
                return Some(text.into_bytes());
            }
            let index: usize = path.trim_start_matches("/s").trim_end_matches(".ts").parse().ok()?;
            segments.get(index).cloned()
        });
        let target = target_texture(&gpu, 320, 240);
        let mut native = Native::open(&format!("{base}/live.m3u8"), &gpu).expect("abrir");
        let started = Instant::now();
        let (mut shown, mut was_live, mut max_position) = (0u32, false, 0.0f64);
        while started.elapsed() < Duration::from_secs(20) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            if native.copy_frame(&target, (0.0, 0.0, 320.0, 240.0), (320, 240), false) {
                shown += 1;
            }
            if native.opened() && native.duration().is_infinite() {
                was_live = true;
                assert!(native.live_window().is_some());
            }
            max_position = max_position.max(native.position());
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended(), "no terminó (imágenes {shown}, posición {max_position:.2})");
        assert!(was_live, "mientras no terminaba la lista, la duración debía ser infinita");
        assert!((native.duration() - 4.0).abs() < 0.01, "duración final {}", native.duration());
        assert!(native.live_window().is_none());
        assert!(shown >= 60, "pocas imágenes: {shown}");
        assert!(max_position > 3.0, "el reloj no avanzó: {max_position}");
        native.close();
    }

    // Annex B access unit as one length-prefixed sample, without the parameter sets.
    fn avcc_sample(unit: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nal_units(unit) {
            if matches!(nal.first().map(|b| b & 0x1F), Some(7 | 8)) {
                continue;
            }
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    #[ignore = "needs a GPU, an H.264 encoder and an audio device (plays a very quiet tone)"]
    fn plays_fragmented_mp4_with_sound() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (320u32, 240u32);
        let units = encode_units(&gpu, w, h, 90);
        let ranges = key_ranges(&units);
        assert!(ranges.len() >= 3, "pocos grupos de imágenes: {}", ranges.len());
        let sets: Vec<&[u8]> = nal_units(&units[0]);
        let sps = sets.iter().find(|n| n[0] & 0x1F == 7).expect("SPS");
        let pps = sets.iter().find(|n| n[0] & 0x1F == 8).expect("PPS");
        let init = mp4::tests::init_segment_with(sps, pps);
        let audio = adts_frames(3);
        assert!(audio.len() >= 126, "pocos paquetes de audio: {}", audio.len());
        let per_segment = 43;
        // Both streams start 10 s into the stream, as real ones often do.
        let (video_base, audio_base) = (900_000u64, 10 * 44_100u64);
        let segments: Vec<Vec<u8>> = ranges[..3]
            .iter()
            .enumerate()
            .map(|(n, r)| {
                let video: Vec<_> = r.clone().map(|i| (avcc_sample(&units[i]), 3000u32, 0i32)).collect();
                let sound: Vec<_> = audio[n * per_segment..(n + 1) * per_segment].iter().map(|f| (f[7..].to_vec(), 1024u32, 0i32)).collect();
                mp4::tests::fragment(&[
                    (1, video_base + r.start as u64 * 3000, video),
                    (2, audio_base + (n * per_segment * 1024) as u64, sound),
                ])
            })
            .collect();
        let list = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\ns0.m4s\n#EXTINF:1.0,\ns1.m4s\n#EXTINF:1.0,\ns2.m4s\n#EXT-X-ENDLIST\n";
        let url = serve(vec![
            ("/list.m3u8".into(), list.as_bytes().to_vec()),
            ("/init.mp4".into(), init),
            ("/s0.m4s".into(), segments[0].clone()),
            ("/s1.m4s".into(), segments[1].clone()),
            ("/s2.m4s".into(), segments[2].clone()),
        ]);
        let target = target_texture(&gpu, w, h);
        let mut native = Native::open(&url, &gpu).expect("abrir");
        let started = Instant::now();
        let (mut shown, mut max_position, mut used_audio_clock) = (0u32, 0.0f64, false);
        while started.elapsed() < Duration::from_secs(14) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            if native.opened() && native.copy_frame(&target, (0.0, 0.0, w as f32, h as f32), (w, h), false) {
                shown += 1;
            }
            if let Some(p) = &native.pipe {
                used_audio_clock |= p.has_audio.load(Ordering::Acquire) && p.audio_clock_us.load(Ordering::Acquire) != NOT_STARTED;
            }
            max_position = max_position.max(native.position());
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(native.ended(), "no terminó (imágenes {shown}, posición {max_position:.2})");
        assert!(used_audio_clock, "el reloj de audio nunca empezó");
        assert_eq!(native.video_size(), Some((w, h)));
        assert!((native.duration() - 3.0).abs() < 0.01);
        assert!(shown >= 45, "pocas imágenes: {shown}");
        assert!(max_position > 2.3, "el reloj no avanzó: {max_position}");
        let image = crate::media::screenshot::read_texture(&gpu, &target).expect("leer");
        let lit = image.pixels.chunks_exact(4).filter(|p| p[0] > 20 || p[1] > 20 || p[2] > 20).count();
        assert!(lit > 1000, "la imagen está vacía ({lit} píxeles)");
        native.close();
    }

    #[test]
    #[ignore = "needs a GPU with an H.264 encoder"]
    fn double_speed_halves_the_time_of_a_stream_without_sound() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let url = two_quality_stream(&gpu, Duration::ZERO);
        let target = target_texture(&gpu, 320, 240);
        let mut native = Native::open(&url, &gpu).expect("abrir");
        native.set_rate(2.0);
        assert_eq!(native.rate, 2.0);
        let mut first = None;
        while first.is_none_or(|t: Instant| t.elapsed() < Duration::from_secs(10)) && !native.ended() {
            native.poll();
            assert!(!native.failed(), "falló: {}", native.error());
            if native.copy_frame(&target, (0.0, 0.0, 320.0, 240.0), (320, 240), false) {
                first.get_or_insert_with(Instant::now);
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let played = first.expect("sin imágenes").elapsed().as_secs_f64();
        let duration = native.duration();
        assert!(native.ended());
        assert!(played < duration / 2.0 + 0.8, "{duration} s de vídeo a 2x tardaron {played:.2} s");
        assert!(played > duration / 2.0 - 0.4, "demasiado rápido: {played:.2} s");
        native.close();
    }
}

mod access;
mod audio;
mod dacp;
mod devices;
mod capture;
mod handlers;
mod metadata;
mod mirror;
mod proto;
mod session;
mod txt;
mod photo;
mod video;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use capture::CaptureOptions;
use capture::Capture;
use mirror::Prewarm;
pub use dacp::RemoteCommand;
pub use metadata::NowPlaying;
pub use devices::KeepWindow;
pub use access::{AccessConfig, AccessPolicy, Decision, PinConfig, decide, forget_paired_device};
pub use rustplay_core::media::player::PlayerTexts as VideoTexts;
pub use rustplay_core::media::video::Scaling;

use rustplay_core::identity::Identity;
use rustplay_core::mdns::{self, Registration, ServiceSpec};
use rustplay_core::media::audio::AudioCodec;
use rustplay_core::rtsp::ServerCodec;
use rustplay_core::{AIRPLAY_SERVICE, Error, RAOP_SERVICE, Result};
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use handlers::Context;
use session::Session;

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub name: String,
    pub port: u16,
    pub audio: bool,
    pub video: bool,
    pub display_width: u32,
    pub display_height: u32,
    pub max_fps: u32,
    pub window: WindowOptions,
    pub texts: MirrorTexts,
    // WASAPI endpoint ID of the speakers for received audio; `None` follows the system default.
    pub audio_output: Option<String>,
    // Extra playout delay for received audio (lip-sync), clamped to ±AUDIO_DELAY_MAX_MS.
    pub audio_delay_ms: i32,
    // How long a mirror window waits for its device after an unexpected disconnect.
    pub keep_window: KeepWindow,
    // Several senders may mirror at once, each in its own window. When false, a device that
    // starts streaming disconnects the previous one.
    pub multiple_devices: bool,
    pub capture: CaptureOptions,
    // Accepts videos that apps send by URL (POST /play), shown in their own player window.
    // Advertised on the network, so changing it needs a restart.
    pub app_videos: bool,
    // Accepts photos and slideshows that apps send (PUT /photo, /slideshows), shown in a viewer window.
    pub app_photos: bool,
    pub video_texts: VideoTexts,
    // Ranked language codes for the audio of HLS videos (e.g. "es").
    pub video_languages: Vec<String>,
    // Listen only on 127.0.0.1 (tests), which never triggers a firewall prompt.
    pub loopback_only: bool,
    // Announced over mDNS: changing it needs a restart.
    pub pin: PinConfig,
    pub access: AccessConfig,
}

// Texts shown by the mirror window, already in the user's language (the app's translation files).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorTexts {
    pub lock_title: String,
    // "{device}" is replaced with the sender's name.
    pub lock_message: String,
    // Used for {device} when the sender did not give its name.
    pub unknown_device: String,
    // Tooltips of the hover controls.
    pub fullscreen: String,
    pub exit_fullscreen: String,
    pub open_app: String,
    pub stop: String,
    pub record: String,
    pub stop_recording: String,
    pub screenshot: String,
    // Names of the keyboard-shortcut actions shown in toasts, and the words for on and off.
    pub toast_fullscreen: String,
    pub toast_recording: String,
    pub toast_screenshot: String,
    pub toast_mute: String,
    pub toast_blur: String,
    pub toast_cinema: String,
    pub toast_lock: String,
    pub toast_stats: String,
    pub toast_topmost: String,
    pub toast_on: String,
    pub toast_off: String,
    // "{width}", "{height}", "{fps}" and "{decoder}" are filled in.
    pub stats: String,
    pub decoder_hardware: String,
    pub decoder_software: String,
    // Backdrop of a window whose sender disconnected (screen off for a while, network lost);
    // "{device}" is replaced as in `lock_message`.
    pub paused_title: String,
    pub paused_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowOptions {
    pub fullscreen: bool,
    pub always_on_top: bool,
    pub lock_screen_backdrop: bool,
    pub scaling: Scaling,
    pub vsync: bool,
    pub high_quality_scaling: bool,
    // Full screen / stop / open-the-app buttons while the pointer moves over the window.
    pub controls: bool,
    // Adds the "open the app" button; pressing it sends RxEvent::OpenApp.
    pub open_app_button: bool,
    pub stats: bool,
    // Cinema mode: no title bar or frame.
    pub borderless: bool,
    // The bars around the picture show a blurred copy of it instead of black.
    pub blur_background: bool,
    // Moving and resizing the window is disabled.
    pub lock_position: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            fullscreen: false,
            always_on_top: false,
            lock_screen_backdrop: true,
            scaling: Scaling::Fit,
            vsync: false,
            high_quality_scaling: false,
            controls: true,
            open_app_button: false,
            stats: false,
            borderless: false,
            blur_background: true,
            lock_position: false,
        }
    }
}

// Receiver options that apply without restarting it (everything but the name and the port).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveConfig {
    pub audio: bool,
    pub video: bool,
    pub display_width: u32,
    pub display_height: u32,
    pub max_fps: u32,
    pub window: WindowOptions,
    pub texts: MirrorTexts,
    pub audio_output: Option<String>,
    pub audio_delay_ms: i32,
    pub keep_window: KeepWindow,
    pub multiple_devices: bool,
    pub access: AccessConfig,
    pub capture: CaptureOptions,
    pub video_texts: VideoTexts,
}

pub const AUDIO_DELAY_MAX_MS: i32 = rustplay_core::media::audio::MAX_EXTRA_DELAY_MS as i32;

impl From<&ReceiverConfig> for LiveConfig {
    fn from(config: &ReceiverConfig) -> Self {
        Self {
            audio: config.audio,
            video: config.video,
            display_width: config.display_width.max(1),
            display_height: config.display_height.max(1),
            max_fps: config.max_fps.max(1),
            window: config.window,
            texts: config.texts.clone(),
            audio_output: config.audio_output.clone().filter(|id| !id.is_empty()),
            audio_delay_ms: config.audio_delay_ms.clamp(-AUDIO_DELAY_MAX_MS, AUDIO_DELAY_MAX_MS),
            keep_window: config.keep_window,
            multiple_devices: config.multiple_devices,
            access: config.access.clone(),
            capture: config.capture.clone(),
            video_texts: config.video_texts.clone(),
        }
    }
}

pub(crate) struct Live {
    config: RwLock<LiveConfig>,
    generation: AtomicU64,
    pub capture: Arc<Capture>,
}

impl Live {
    pub fn new(config: LiveConfig) -> Self {
        Self { config: RwLock::new(config), generation: AtomicU64::new(0), capture: Arc::default() }
    }

    pub fn get(&self) -> LiveConfig {
        self.config.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    // Increases on every change, so open windows can notice it cheaply.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn update(&self, change: impl FnOnce(&mut LiveConfig)) {
        let mut config = self.config.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = config.clone();
        change(&mut config);
        if *config != before {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            name: String::from("Windows"),
            port: 7000,
            audio: true,
            video: true,
            display_width: 1920,
            display_height: 1080,
            max_fps: 60,
            window: WindowOptions::default(),
            texts: MirrorTexts::default(),
            audio_output: None,
            audio_delay_ms: 0,
            keep_window: KeepWindow::default(),
            multiple_devices: false,
            capture: CaptureOptions::default(),
            app_videos: false,
            app_photos: false,
            video_texts: VideoTexts::default(),
            video_languages: Vec::new(),
            loopback_only: false,
            pin: PinConfig::default(),
            access: AccessConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum RxEvent {
    Listening {
        port: u16,
    },
    Connected {
        peer: SocketAddr,
        name: Option<String>,
    },
    ClientName {
        peer: SocketAddr,
        name: String,
    },
    AudioStarted {
        peer: SocketAddr,
        codec: AudioCodec,
    },
    MirrorStarted {
        peer: SocketAddr,
        width: u32,
        height: u32,
    },
    MirrorStopped {
        peer: SocketAddr,
    },
    MirrorSuspended {
        peer: SocketAddr,
    },
    MirrorResumed {
        peer: SocketAddr,
    },
    // An app sent a video (POST /play) and a player window opened for it.
    VideoStarted {
        peer: SocketAddr,
    },
    VideoStopped,
    Volume {
        db: f32,
    },
    StreamStopped {
        peer: SocketAddr,
    },
    // An app sent photos or a slideshow and the viewer window opened for them.
    PhotosStarted {
        peer: SocketAddr,
    },
    PhotosStopped,
    // The user asked from the mirror window to bring the app to the front.
    OpenApp,
    // Track details sent by the device while it streams audio (title, artwork, progress...).
    // Sent again whenever something changes; `StreamStopped` or `Disconnected` end it.
    NowPlaying(Box<NowPlaying>),
    // A recording started or ended (a new file after a resolution change keeps it active).
    RecordingState {
        active: bool,
    },
    // A recording file was completed.
    RecordingSaved {
        path: std::path::PathBuf,
    },
    RecordingFailed {
        message: String,
    },
    ScreenshotSaved {
        path: std::path::PathBuf,
    },
    ScreenshotFailed {
        message: String,
    },
    Disconnected {
        peer: SocketAddr,
    },
    // A sender identified itself in its first SETUP (before the access check).
    DeviceSeen {
        peer: SocketAddr,
        device_id: String,
        name: Option<String>,
        model: Option<String>,
    },
    // The policy is "ask": answer with `ReceiverHandle::answer_access(id, allow)` within
    // `ACCESS_TIMEOUT`, or the connection is refused.
    AccessRequest {
        id: u64,
        peer: SocketAddr,
        device_id: String,
        name: Option<String>,
    },
    // The request timed out or was answered elsewhere; close any prompt for it.
    AccessResolved {
        id: u64,
    },
    AccessDenied {
        peer: SocketAddr,
        device_id: String,
        name: Option<String>,
    },
    // PIN pairing: show `pin` until `PinFinished` for the same peer (or its disconnection).
    PinShown {
        peer: SocketAddr,
        pin: String,
    },
    PinFinished {
        peer: SocketAddr,
        paired: bool,
    },
}

pub const ACCESS_TIMEOUT: std::time::Duration = access::ASK_TIMEOUT;

pub struct ReceiverHandle {
    events: mpsc::Receiver<RxEvent>,
    cancel: CancellationToken,
    accept_task: JoinHandle<()>,
    registrations: Vec<Registration>,
    live: Arc<Live>,
    prewarm: Arc<Prewarm>,
    remotes: Arc<dacp::Remotes>,
    windows: Arc<devices::Windows<mirror::Attach>>,
    access: Arc<access::AccessState>,
    video: Arc<video::Hub>,
    photo: Arc<photo::Hub>,
}

impl ReceiverHandle {
    pub fn events(&mut self) -> &mut mpsc::Receiver<RxEvent> {
        &mut self.events
    }

    // Applies new options at once, also to an open mirror window. The name and the port are
    // announced on the network, so changing them needs a restart.
    pub fn update(&self, config: &ReceiverConfig) {
        let next = LiveConfig::from(config);
        let video = next.video;
        self.live.update(|current| *current = next);
        if video {
            self.prewarm.fill();
        }
    }

    // Sends a DACP command (play/pause, next...) to the device that is streaming audio. Returns
    // false when no connected device offered remote control. Runs in the background; must be
    // called from inside the tokio runtime.
    pub fn remote(&self, command: RemoteCommand) -> bool {
        self.remotes.send(command)
    }

    // Answers an `RxEvent::AccessRequest`; false when it already timed out.
    pub fn answer_access(&self, id: u64, allow: bool) -> bool {
        self.access.answer(id, allow)
    }

    // Starts or stops recording the mirrored screen. False when nothing is being mirrored.
    pub fn toggle_recording(&self) -> bool {
        self.live.capture.request_record()
    }

    // Saves the current mirrored picture as a PNG. False when nothing is being mirrored.
    pub fn take_screenshot(&self) -> bool {
        self.live.capture.request_screenshot()
    }

    pub fn is_mirroring(&self) -> bool {
        self.live.capture.is_mirroring()
    }

    // How long the running recording has been going, if any.
    pub fn recording_elapsed(&self) -> Option<std::time::Duration> {
        self.live.capture.elapsed()
    }

    pub async fn shutdown(self) -> Result<()> {
        self.cancel.cancel();
        self.video.stop_all();
        self.photo.stop_all();
        drop(self.registrations);
        self.accept_task
            .await
            .map_err(|e| Error::protocol(format!("fallo al detener el receptor: {e}")))?;
        // Windows still waiting for their device close with the receiver.
        for thread in self.windows.close_all() {
            let _ = thread.join();
        }
        Ok(())
    }
}

pub struct Receiver;

impl Receiver {
    pub async fn start(config: ReceiverConfig) -> Result<ReceiverHandle> {
        let identity = Identity::load_or_create()?;
        let public_key = identity.public_key().to_bytes();
        let mut records = txt::build_full(&identity.device_id, &public_key, &identity.pairing_id, config.app_videos, config.app_photos);
        if config.pin.required {
            txt::require_pin(&mut records);
        }

        let listener = TcpListener::bind((if config.loopback_only { "127.0.0.1" } else { "0.0.0.0" }, config.port)).await?;
        let port = listener.local_addr()?.port();

        let specs = [
            ServiceSpec {
                instance: txt::raop_instance(&identity.device_id, &config.name),
                service_type: RAOP_SERVICE,
                port,
                txt: records.raop.clone(),
            },
            ServiceSpec {
                instance: config.name.clone(),
                service_type: AIRPLAY_SERVICE,
                port,
                txt: records.airplay.clone(),
            },
        ];
        let mut registrations = Vec::with_capacity(specs.len());
        for spec in specs {
            match mdns::register(spec).await {
                Ok(registration) => registrations.push(registration),
                Err(Error::NotImplemented(what)) => {
                    tracing::warn!(
                        "el anuncio mDNS no está disponible ({what}); se continúa sin anunciar"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        let (tx, rx) = mpsc::channel(64);
        let live = Arc::new(Live::new(LiveConfig::from(&config)));
        let prewarm = Prewarm::new();
        let remotes = Arc::new(dacp::Remotes::default());
        if config.video {
            prewarm.fill();
        }
        let windows = Arc::new(devices::Windows::default());
        let clients = rustplay_core::identity::client_pairings_path()
            .inspect_err(|e| tracing::warn!("no se recordarán los dispositivos emparejados: {e}"))
            .ok();
        let access = Arc::new(access::AccessState::new(config.pin.clone(), clients));
        let video = video::Hub::new(
            config.app_videos,
            port,
            config.name.clone(),
            txt::device_id_colon(&identity.device_id),
            config.video_languages.clone(),
            video::window_factory(Arc::clone(&live)),
            tx.clone(),
        );
        let photo = photo::Hub::new(
            config.app_photos,
            config.name.clone(),
            txt::device_id_colon(&identity.device_id),
            photo::window_factory(Arc::clone(&live)),
            tx.clone(),
        );
        let ctx = Arc::new(Context {
            name: config.name,
            device_id: identity.device_id,
            public_key,
            pairing_id: identity.pairing_id.clone(),
            identity,
            txt_raop: records.raop,
            txt_airplay: records.airplay,
            events: tx.clone(),
            live: Arc::clone(&live),
            prewarm: Arc::clone(&prewarm),
            remotes: Arc::clone(&remotes),
            sessions: Arc::new(devices::Sessions::default()),
            windows: Arc::clone(&windows),
            access: Arc::clone(&access),
            video: Arc::clone(&video),
            photo: Arc::clone(&photo),
        });
        let cancel = CancellationToken::new();
        let _ = tx.try_send(RxEvent::Listening { port });
        let accept_task = tokio::spawn(accept_loop(listener, ctx, tx, cancel.clone()));

        Ok(ReceiverHandle {
            events: rx,
            cancel,
            accept_task,
            registrations,
            live,
            prewarm,
            remotes,
            windows,
            access,
            video,
            photo,
        })
    }
}

async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<Context>,
    events: mpsc::Sender<RxEvent>,
    cancel: CancellationToken,
) {
    let mut connections = JoinSet::new();
    // On a network-wide listener, connections from this PC are RustPlay's own video player
    // reading the HLS playlists: they are not devices and must not show up as connected.
    let local_listener = listener.local_addr().is_ok_and(|a| a.ip().is_loopback());
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let ctx = Arc::clone(&ctx);
                    let events = events.clone();
                    let cancel = cancel.child_token();
                    let announce = local_listener || !peer.ip().is_loopback();
                    connections.spawn(async move {
                        if announce {
                            let _ = events.try_send(RxEvent::Connected { peer, name: None });
                        }
                        if let Err(e) = serve_connection(stream, peer, &ctx, cancel).await {
                            tracing::debug!(%peer, "conexión cerrada con error: {e}");
                        }
                        if announce {
                            let _ = events.try_send(RxEvent::Disconnected { peer });
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("error al aceptar una conexión: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    while connections.join_next().await.is_some() {}
}

async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: &Context,
    cancel: CancellationToken,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut session = Session::new(ctx, peer, &cancel);
    let result = connection_loop(&mut stream, &mut session, &cancel).await;
    session.finish();
    result
}

async fn connection_loop(
    stream: &mut TcpStream,
    session: &mut Session<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut codec = ServerCodec;
    let mut inbound = BytesMut::with_capacity(8 * 1024);
    let mut outbound = BytesMut::with_capacity(8 * 1024);
    let mut watchdog = tokio::time::interval(session::WATCHDOG_TICK);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        while let Some(request) = codec.decode(&mut inbound)? {
            let response = session.handle(&request).await;
            let close = response
                .headers
                .get("Connection")
                .is_some_and(|v| v.eq_ignore_ascii_case("close"));
            outbound.clear();
            codec.encode(response, &mut outbound)?;
            stream.write_all(&outbound).await?;
            if close {
                return Ok(());
            }
            if let Some(reverse) = session.take_reverse() {
                if session.take_photo_reverse() {
                    let hub = session.photo();
                    return photo::serve_reverse(stream, inbound, hub, reverse, session.peer(), cancel).await;
                }
                let hub = session.video();
                return video::serve_reverse(stream, inbound, hub, reverse, session.peer(), cancel).await;
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            read = stream.read_buf(&mut inbound) => {
                if read? == 0 {
                    return Ok(());
                }
            }
            _ = watchdog.tick() => {
                // iOS stops its /feedback heartbeat when it goes to sleep, sometimes without
                // closing the connection.
                if session.heartbeat_lost() {
                    return Ok(());
                }
            }
        }
    }
}

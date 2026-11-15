use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rustplay_core::Error;
use rustplay_core::identity::PairingStore;
use rustplay_core::mdns::DiscoveredDevice;
use rustplay_rx::{Receiver, ReceiverConfig, RemoteCommand, RxEvent};
use rustplay_tx::{MirrorOptions, Session};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc as tmpsc, oneshot};
use tokio::task::JoinHandle;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

use crate::i18n::{t, tf};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_BACKEND: u32 = WM_APP + 1;

// Seconds to wait before each reconnection attempt; the whole schedule adds up to about a minute.
const RETRY_DELAYS_SECS: [u64; 8] = [1, 2, 5, 10, 10, 10, 10, 10];

// Delay before reconnection attempt `attempt` (0-based); None once the schedule is exhausted.
pub fn retry_delay(attempt: usize) -> Option<Duration> {
    RETRY_DELAYS_SECS.get(attempt).map(|secs| Duration::from_secs(*secs))
}

pub enum Command {
    Discover,
    Mirror { device: Box<DiscoveredDevice>, options: MirrorOptions, reconnect: bool },
    StopMirror,
    Pin(Option<String>),
    StartReceiver(ReceiverConfig),
    // Options that apply without a restart (everything but the name).
    UpdateReceiver(ReceiverConfig),
    // DACP command for the device that streams audio to the receiver.
    Remote(RemoteCommand),
    StopReceiver,
    // Answer to `RxEvent::AccessRequest`.
    AnswerAccess { id: u64, allow: bool },
    // Recording and screenshots of the receiver's mirror window.
    Capture(CaptureAction),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy)]
pub enum CaptureAction {
    ToggleRecording,
    Screenshot,
}

#[derive(Debug)]
pub enum Event {
    Discovering,
    Devices(Vec<DiscoveredDevice>),
    DiscoveryFailed(String),
    Connecting { key: String },
    // The link dropped; attempt `attempt` (1-based) is scheduled after the backoff delay.
    Reconnecting { key: String, attempt: u32 },
    PinRequired { name: String },
    Mirroring { key: String, width: u32, height: u32, encoder: Option<String> },
    MirrorEnded { key: String, error: Option<String> },
    Receiver(RxEvent),
    ReceiverStopped { error: Option<String> },
}

#[derive(Clone)]
struct Sink {
    events: mpsc::Sender<Event>,
    hwnd: isize,
}

impl Sink {
    fn emit(&self, event: Event) {
        if self.events.send(event).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_BACKEND, WPARAM(0), LPARAM(0));
            }
        }
    }
}

pub struct Backend {
    runtime: Runtime,
    commands: tmpsc::UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
}

impl Backend {
    pub fn start(hwnd: HWND) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rustplay-net")
            .enable_all()
            .build()?;
        let (commands, rx) = tmpsc::unbounded_channel();
        let (tx, events) = mpsc::channel();
        let sink = Sink { events: tx, hwnd: hwnd.0 as isize };
        runtime.spawn(run(rx, sink));
        Ok(Self { runtime, commands, events })
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn poll(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn shutdown(self) {
        let (done, wait) = oneshot::channel();
        self.send(Command::Shutdown(done));
        let _ = self.runtime.block_on(tokio::time::timeout(Duration::from_secs(4), wait));
        self.runtime.shutdown_timeout(Duration::from_millis(500));
    }
}

pub fn device_key(device: &DiscoveredDevice) -> String {
    device
        .device_id()
        .map(str::to_owned)
        .or_else(|| device.instance.split_once('@').map(|(mac, _)| mac.to_owned()))
        .unwrap_or_else(|| device.instance.clone())
        .to_ascii_uppercase()
}

pub fn describe(error: &Error) -> String {
    match error {
        Error::Timeout => t("error.timeout").to_owned(),
        Error::Io(e) if e.kind() == io::ErrorKind::AddrInUse => {
            t("error.port_in_use").to_owned()
        }
        Error::Io(e) if e.kind() == io::ErrorKind::PermissionDenied => tf("error.access_denied", &[("error", e)]),
        _ => {
            let text = error.to_string();
            let mut chars = text.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => text,
            }
        }
    }
}

type PinSlot = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

enum ReceiverMsg {
    Config(Box<ReceiverConfig>),
    Answer { id: u64, allow: bool },
}

struct Task {
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
    remote: Option<tmpsc::UnboundedSender<RemoteCommand>>,
    updates: Option<tmpsc::UnboundedSender<ReceiverMsg>>,
    captures: Option<tmpsc::UnboundedSender<CaptureAction>>,
}

impl Task {
    fn running(&self) -> bool {
        !self.handle.is_finished()
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn run(mut commands: tmpsc::UnboundedReceiver<Command>, sink: Sink) {
    let pin: PinSlot = Arc::new(Mutex::new(None));
    let mut mirror: Option<Task> = None;
    let mut receiver: Option<Task> = None;
    let mut discovery: Option<JoinHandle<()>> = None;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Discover => {
                if discovery.as_ref().is_none_or(JoinHandle::is_finished) {
                    discovery = Some(tokio::spawn(discover(sink.clone())));
                }
            }
            Command::Mirror { device, options, reconnect } => {
                if mirror.as_ref().is_some_and(Task::running) {
                    continue;
                }
                let (stop, stopped) = oneshot::channel();
                let handle = tokio::spawn(mirror_task(*device, options, reconnect, stopped, Arc::clone(&pin), sink.clone()));
                mirror = Some(Task { stop: Some(stop), handle, updates: None, remote: None, captures: None });
            }
            Command::StopMirror => {
                cancel_pin(&pin);
                if let Some(task) = mirror.as_mut() {
                    task.stop();
                }
            }
            Command::Pin(value) => {
                if let Some(reply) = pin.lock().ok().and_then(|mut slot| slot.take()) {
                    let _ = reply.send(value);
                }
            }
            Command::StartReceiver(config) => {
                if receiver.as_ref().is_some_and(Task::running) {
                    continue;
                }
                let (stop, stopped) = oneshot::channel();
                let (updates, updated) = tmpsc::unbounded_channel();
                let (remote, remotes) = tmpsc::unbounded_channel();
                let (captures, captured) = tmpsc::unbounded_channel();
                // The mDNS registration is not Send, so the receiver is driven from a blocking thread.
                let runtime = tokio::runtime::Handle::current();
                let sink = sink.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    runtime.block_on(receiver_task(config, stopped, updated, remotes, captured, sink))
                });
                receiver = Some(Task { stop: Some(stop), handle, updates: Some(updates), remote: Some(remote), captures: Some(captures) });
            }
            Command::UpdateReceiver(config) => {
                if let Some(updates) = receiver.as_ref().filter(|t| t.running()).and_then(|t| t.updates.as_ref()) {
                    let _ = updates.send(ReceiverMsg::Config(Box::new(config)));
                }
            }
            Command::AnswerAccess { id, allow } => {
                if let Some(updates) = receiver.as_ref().filter(|t| t.running()).and_then(|t| t.updates.as_ref()) {
                    let _ = updates.send(ReceiverMsg::Answer { id, allow });
                }
            }
            Command::Remote(command) => {
                if let Some(remote) = receiver.as_ref().filter(|t| t.running()).and_then(|t| t.remote.as_ref()) {
                    let _ = remote.send(command);
                }
            }
            Command::Capture(action) => {
                if let Some(captures) = receiver.as_ref().filter(|t| t.running()).and_then(|t| t.captures.as_ref()) {
                    let _ = captures.send(action);
                }
            }
            Command::StopReceiver => {
                if let Some(task) = receiver.as_mut() {
                    task.stop();
                }
            }
            Command::Shutdown(done) => {
                cancel_pin(&pin);
                for task in [mirror.take(), receiver.take()].into_iter().flatten() {
                    let mut task = task;
                    task.stop();
                    let _ = tokio::time::timeout(Duration::from_secs(3), task.handle).await;
                }
                let _ = done.send(());
                return;
            }
        }
    }
}

fn cancel_pin(pin: &PinSlot) {
    if let Some(reply) = pin.lock().ok().and_then(|mut slot| slot.take()) {
        let _ = reply.send(None);
    }
}

async fn discover(sink: Sink) {
    sink.emit(Event::Discovering);
    match rustplay_tx::discover(Duration::from_secs(3)).await {
        Ok(devices) => {
            let names: Vec<&str> = devices.iter().map(DiscoveredDevice::display_name).collect();
            tracing::info!("búsqueda terminada: {} dispositivo(s) {names:?}", devices.len());
            sink.emit(Event::Devices(devices));
        }
        Err(e) => {
            tracing::warn!("no se pudo buscar dispositivos: {e}");
            sink.emit(Event::DiscoveryFailed(describe(&e)));
        }
    }
}

async fn open_stream<F, Fut>(
    device: &DiscoveredDevice,
    options: MirrorOptions,
    ask: F,
) -> Result<(Session, rustplay_tx::StreamHandle), Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String, Error>>,
{
    let mut store = PairingStore::load()?;
    let mut session = Session::connect(device, &mut store, ask).await?;
    match session.start_mirror(options).await {
        Ok(stream) => Ok((session, stream)),
        Err(e) => {
            let _ = session.close().await;
            Err(e)
        }
    }
}

async fn mirror_task(
    device: DiscoveredDevice,
    options: MirrorOptions,
    reconnect: bool,
    mut stop: oneshot::Receiver<()>,
    pin: PinSlot,
    sink: Sink,
) {
    let key = device_key(&device);
    let name = device.display_name().to_owned();
    tracing::info!(
        "conectando con «{name}» para duplicar {} ({} fps, audio {}, reconexión {})",
        options.window.map_or_else(|| format!("la pantalla (monitor {})", options.monitor), |w| format!("la ventana {w:#x}")),
        options.fps,
        options.audio,
        reconnect
    );
    sink.emit(Event::Connecting { key: key.clone() });
    let ask = {
        let sink = sink.clone();
        let name = name.clone();
        move || {
            let (reply, answer) = oneshot::channel();
            if let Ok(mut slot) = pin.lock() {
                *slot = Some(reply);
            }
            tracing::info!("«{name}» pide un PIN de emparejamiento");
            sink.emit(Event::PinRequired { name });
            async move {
                match answer.await {
                    Ok(Some(pin)) if !pin.trim().is_empty() => Ok(pin.trim().to_owned()),
                    _ => Err(Error::Pairing(String::from("se canceló el emparejamiento"))),
                }
            }
        }
    };
    let started = tokio::select! {
        result = open_stream(&device, options, ask) => result,
        _ = &mut stop => {
            sink.emit(Event::MirrorEnded { key, error: None });
            return;
        }
    };
    let (mut session, mut stream) = match started {
        Ok(started) => started,
        Err(e) => {
            tracing::warn!("no se pudo duplicar la pantalla en «{name}»: {e}");
            sink.emit(Event::MirrorEnded { key, error: Some(describe(&e)) });
            return;
        }
    };
    let mut attempt = 0usize;
    loop {
        let (width, height) = stream.video_size().unwrap_or((0, 0));
        sink.emit(Event::Mirroring {
            key: key.clone(),
            width,
            height,
            encoder: stream.encoder_name().map(str::to_owned),
        });
        let failure = tokio::select! {
            _ = &mut stop => None,
            () = stream.closed() => Some(match stream.failure() {
                Some(reason) => tf("error.mirror_stopped", &[("reason", &reason)]),
                None => tf("error.not_responding", &[("name", &name)]),
            }),
        };
        let _ = stream.stop().await;
        let _ = session.close().await;
        let Some(reason) = failure else {
            tracing::info!("duplicación en «{name}» detenida");
            sink.emit(Event::MirrorEnded { key, error: None });
            return;
        };
        tracing::warn!("duplicación en «{name}» terminada: {reason}");
        if !reconnect {
            sink.emit(Event::MirrorEnded { key, error: Some(reason) });
            return;
        }
        // Retries use the stored pairing only: a PIN prompt in the middle of a reconnection is useless.
        let reopened = loop {
            let Some(delay) = retry_delay(attempt) else {
                tracing::warn!("se agotaron los intentos de reconexión con «{name}»");
                sink.emit(Event::MirrorEnded { key, error: Some(tf("error.reconnect_failed", &[("name", &name)])) });
                return;
            };
            attempt += 1;
            tracing::info!("reconectando con «{name}» en {} s (intento {attempt})", delay.as_secs());
            sink.emit(Event::Reconnecting { key: key.clone(), attempt: attempt as u32 });
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = &mut stop => {
                    sink.emit(Event::MirrorEnded { key, error: None });
                    return;
                }
            }
            let no_pin = || async { Err(Error::Pairing(String::from("hay que emparejar de nuevo"))) };
            let result = tokio::select! {
                result = open_stream(&device, options, no_pin) => result,
                _ = &mut stop => {
                    sink.emit(Event::MirrorEnded { key, error: None });
                    return;
                }
            };
            match result {
                Ok(pair) => break pair,
                Err(e @ Error::Pairing(_)) => {
                    sink.emit(Event::MirrorEnded { key, error: Some(describe(&e)) });
                    return;
                }
                Err(e) => tracing::warn!("no se pudo reconectar con «{name}»: {e}"),
            }
        };
        (session, stream) = reopened;
        attempt = 0;
    }
}

async fn receiver_task(
    config: ReceiverConfig,
    mut stop: oneshot::Receiver<()>,
    mut updates: tmpsc::UnboundedReceiver<ReceiverMsg>,
    mut remotes: tmpsc::UnboundedReceiver<RemoteCommand>,
    mut captures: tmpsc::UnboundedReceiver<CaptureAction>,
    sink: Sink,
) {
    tracing::info!("iniciando el receptor «{}» (audio {}, vídeo {})", config.name, config.audio, config.video);
    let mut handle = match Receiver::start(config).await {
        Ok(handle) => handle,
        Err(e) => {
            sink.emit(Event::ReceiverStopped { error: Some(describe(&e)) });
            return;
        }
    };
    loop {
        tokio::select! {
            _ = &mut stop => break,
            Some(command) = remotes.recv() => {
                tracing::info!("control remoto: {command:?}");
                handle.remote(command);
            }
            Some(message) = updates.recv() => match message {
                ReceiverMsg::Config(config) => {
                    handle.update(&config);
                    tracing::info!("cambios del receptor aplicados sin reiniciarlo");
                }
                ReceiverMsg::Answer { id, allow } => {
                    if !handle.answer_access(id, allow) {
                        tracing::info!("la respuesta de acceso llegó tarde (petición {id})");
                    }
                }
            },
            Some(action) = captures.recv() => {
                let accepted = match action {
                    CaptureAction::ToggleRecording => handle.toggle_recording(),
                    CaptureAction::Screenshot => handle.take_screenshot(),
                };
                if !accepted {
                    tracing::debug!("captura ignorada: no hay duplicación de pantalla en curso");
                }
            }
            event = handle.events().recv() => match event {
                Some(event) => {
                log_receiver_event(&event);
                sink.emit(Event::Receiver(event));
            }
                None => break,
            },
        }
    }
    let result = handle.shutdown().await;
    tracing::info!("receptor detenido");
    sink.emit(Event::ReceiverStopped { error: result.err().map(|e| describe(&e)) });
}

fn log_receiver_event(event: &RxEvent) {
    match event {
        RxEvent::Listening { port } => tracing::info!("receptor visible en la red (puerto {port})"),
        RxEvent::Connected { peer, name } => tracing::info!("conexión entrante de {} ({peer})", name.as_deref().unwrap_or("un dispositivo")),
        RxEvent::ClientName { peer, name } => tracing::info!("{peer} se identifica como «{name}»"),
        RxEvent::AudioStarted { peer, codec } => tracing::info!("{peer}: audio recibido ({codec:?})"),
        RxEvent::MirrorStarted { peer, width, height } => tracing::info!("{peer}: duplicación recibida: {width}x{height}"),
        RxEvent::MirrorStopped { peer } => tracing::info!("{peer}: duplicación recibida detenida"),
        RxEvent::Volume { db } => tracing::debug!("volumen del emisor: {db:.1} dB"),
        RxEvent::StreamStopped { peer } => tracing::info!("{peer}: transmisión recibida detenida"),
        RxEvent::OpenApp => tracing::debug!("la ventana de duplicación pide abrir RustPlay"),
        RxEvent::NowPlaying(now) => tracing::debug!(
            "sonando: {} · {} ({:?}/{:?} ms, {})",
            now.title.as_deref().unwrap_or("?"),
            now.artist.as_deref().unwrap_or("?"),
            now.position_ms,
            now.duration_ms,
            if now.playing { "reproduciendo" } else { "en pausa" }
        ),
        RxEvent::Disconnected { peer } => tracing::info!("{peer} se ha desconectado"),
        RxEvent::MirrorSuspended { peer } => tracing::info!("{peer}: el emisor ha apagado la pantalla"),
        RxEvent::MirrorResumed { peer } => tracing::info!("{peer}: el emisor ha encendido la pantalla"),
        RxEvent::DeviceSeen { peer, device_id, name, .. } => {
            tracing::info!("{peer} es el dispositivo {device_id} ({})", name.as_deref().unwrap_or("sin nombre"));
        }
        RxEvent::AccessRequest { peer, device_id, .. } => tracing::info!("se pide permiso para {peer} ({device_id})"),
        RxEvent::AccessResolved { id } => tracing::debug!("petición de acceso {id} cerrada"),
        RxEvent::AccessDenied { peer, device_id, .. } => tracing::warn!("acceso denegado a {peer} ({device_id})"),
        RxEvent::PinShown { peer, .. } => tracing::info!("se muestra el código de emparejamiento a {peer}"),
        RxEvent::PinFinished { peer, paired } => tracing::info!("emparejamiento con código de {peer} terminado (emparejado: {paired})"),
        RxEvent::RecordingState { active } => tracing::debug!("grabación activa: {active}"),
        RxEvent::RecordingSaved { path } => tracing::info!("grabación guardada en {}", path.display()),
        RxEvent::RecordingFailed { message } => tracing::warn!("grabación fallida: {message}"),
        RxEvent::ScreenshotSaved { path } => tracing::info!("captura guardada en {}", path.display()),
        RxEvent::ScreenshotFailed { message } => tracing::warn!("captura fallida: {message}"),
        RxEvent::VideoStarted { peer } => tracing::info!("{peer} envía un vídeo"),
        RxEvent::VideoStopped => tracing::info!("vídeo recibido detenido"),
        RxEvent::PhotosStarted { peer } => tracing::info!("{peer} envía fotos"),
        RxEvent::PhotosStopped => tracing::info!("fotos recibidas detenidas"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_fast_then_levels_off() {
        let secs: Vec<u64> = (0..).map_while(retry_delay).map(|d| d.as_secs()).collect();
        assert_eq!(&secs[..4], [1, 2, 5, 10]);
        assert!(secs.windows(2).all(|w| w[0] <= w[1]), "never shrinks");
        assert_eq!(secs.iter().max(), Some(&10));
    }

    #[test]
    fn backoff_gives_up_after_about_a_minute() {
        let total: u64 = (0..).map_while(retry_delay).map(|d| d.as_secs()).sum();
        assert!((50..=70).contains(&total), "total {total} s");
        assert!(retry_delay(1000).is_none());
    }
}

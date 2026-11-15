use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle as ThreadHandle;
use std::time::{Duration, Instant};

use rustplay_core::Result;
use rustplay_core::crypto::stream::AudioCipher;
use rustplay_core::media::audio::{AudioCodec, AudioFormat, Renderer, create_decoder};
use rustplay_core::rtp::{self, JitterBuffer, RtpHeader};
use tokio::net::UdpSocket;
use tokio::sync::mpsc as async_mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Live;
use crate::proto::{AudioRequest, db_to_linear};

pub const PACKET_MAX: usize = 4096;
const JITTER_CAPACITY: usize = 512;
const POOL_SIZE: usize = 128;
const PREBUFFER_MS: u32 = 100;
const SKIP_AFTER_MS: u32 = 250;
const RESEND_INTERVAL: Duration = Duration::from_millis(15);
const TICK: Duration = Duration::from_millis(10);
const NO_DATA_MARKER: [u8; 4] = [0x00, 0x68, 0x34, 0x00];
const DEFAULT_LATENCY_MS: u32 = 150;

pub fn latency_ms_for(request: &AudioRequest) -> u32 {
    request
        .latency_min
        .map_or(DEFAULT_LATENCY_MS, |samples| {
            (u64::from(samples) * 1000 / u64::from(request.sample_rate.max(1))) as u32
        })
        .clamp(DEFAULT_LATENCY_MS, 500)
}

struct Packet {
    buf: Vec<u8>,
    timestamp: u32,
}

struct RenderShared {
    stop: AtomicBool,
    flush_generation: AtomicU32,
    volume_bits: AtomicU32,
    // Set while another sender's audio has the focus.
    muted: Arc<AtomicBool>,
    // Set while the user muted this device from its mirror window.
    user_muted: Arc<AtomicBool>,
}

enum Command {
    Flush(Option<u16>),
}

pub struct AudioStream {
    pub data_port: u16,
    pub control_port: u16,
    pub codec: AudioCodec,
    pub latency_ms: u32,
    cancel: CancellationToken,
    commands: async_mpsc::UnboundedSender<Command>,
    shared: Arc<RenderShared>,
    task: JoinHandle<()>,
    thread: Option<ThreadHandle<()>>,
}

async fn bind_udp(peer: IpAddr) -> std::io::Result<UdpSocket> {
    UdpSocket::bind((rustplay_core::net::listen_addr(peer), 0)).await
}

impl AudioStream {
    pub async fn start(
        request: &AudioRequest,
        peer: SocketAddr,
        cipher: AudioCipher,
        initial_volume_db: f32,
        user_muted: Arc<AtomicBool>,
        live: Arc<Live>,
        parent: &CancellationToken,
    ) -> Result<Self> {
        let data = bind_udp(peer.ip()).await?;
        let control = bind_udp(peer.ip()).await?;
        let data_port = data.local_addr()?.port();
        let control_port = control.local_addr()?.port();
        let codec = request.codec();
        let frames_per_packet = request.frames_per_packet();
        let latency_ms = latency_ms_for(request);
        let cancel = parent.child_token();
        let shared = Arc::new(RenderShared {
            stop: AtomicBool::new(false),
            flush_generation: AtomicU32::new(0),
            volume_bits: AtomicU32::new(db_to_linear(initial_volume_db).to_bits()),
            muted: Arc::new(AtomicBool::new(false)),
            user_muted,
        });

        let (packet_tx, packet_rx) = mpsc::sync_channel::<Packet>(POOL_SIZE);
        let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            let _ = free_tx.try_send(Vec::with_capacity(PACKET_MAX));
        }

        let format = AudioFormat {
            codec,
            sample_rate: request.sample_rate,
            channels: rustplay_core::media::audio::CHANNELS,
            frames_per_packet,
        };
        let render_shared = Arc::clone(&shared);
        let thread = Some(
            std::thread::Builder::new()
                .name("rustplay-audio-render".into())
                .spawn(move || {
                    render_thread(format, latency_ms, packet_rx, free_tx, render_shared, &live)
                })?,
        );

        let (commands, command_rx) = async_mpsc::unbounded_channel();
        let state = ReceiverState::new(
            codec,
            request,
            frames_per_packet,
            {
                let mut target = peer;
                target.set_port(request.control_port);
                target
            },
            cipher,
            packet_tx,
            free_rx,
            Arc::clone(&shared),
        );
        let task = tokio::spawn(receive_loop(
            state,
            data,
            control,
            command_rx,
            cancel.clone(),
        ));
        Ok(Self {
            data_port,
            control_port,
            codec,
            latency_ms,
            cancel,
            commands,
            shared,
            task,
            thread,
        })
    }

    pub fn flush(&self, next_sequence: Option<u16>) {
        let _ = self.commands.send(Command::Flush(next_sequence));
    }

    pub fn set_volume_db(&self, db: f32) {
        self.shared
            .volume_bits
            .store(db_to_linear(db).to_bits(), Ordering::Relaxed);
    }

    // Silences the stream while true, keeping the sender's volume for later.
    pub fn mute_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.muted)
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ReceiverState {
    codec: AudioCodec,
    cipher: AudioCipher,
    jitter: JitterBuffer,
    packet_tx: SyncSender<Packet>,
    free_rx: Receiver<Vec<u8>>,
    spare: Option<Vec<u8>>,
    shared: Arc<RenderShared>,
    prebuffer_packets: usize,
    playing: bool,
    resend_target: Option<SocketAddr>,
    control_sequence: u16,
    last_resend: Option<(u16, Instant)>,
    dropped: u64,
    received: u64,
    syncs: u64,
    dump: Option<AudioDump<BufWriter<std::fs::File>>>,
}

pub struct AudioDump<W: Write> {
    out: W,
}

impl<W: Write> AudioDump<W> {
    pub fn new(mut out: W, ct: u8, spf: u32, sample_rate: u32) -> std::io::Result<Self> {
        let mut record = [0u8; 17];
        record[4] = ct;
        record[5..9].copy_from_slice(&spf.to_le_bytes());
        record[9..13].copy_from_slice(&sample_rate.to_le_bytes());
        out.write_all(&record[..13])?;
        Ok(Self { out })
    }

    pub fn record(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.out.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.out.write_all(payload)
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

fn open_dump(request: &AudioRequest) -> Option<AudioDump<BufWriter<std::fs::File>>> {
    let path = std::env::var_os("RUSTPLAY_DUMP_AUDIO").filter(|p| !p.is_empty())?;
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("no se pudo abrir el archivo de volcado de audio: {e}");
            return None;
        }
    };
    let dump = AudioDump::new(
        BufWriter::with_capacity(64 * 1024, file),
        request.ct,
        request.spf,
        request.sample_rate,
    );
    match dump {
        Ok(dump) => {
            tracing::info!("volcando el audio descifrado a {}", path.to_string_lossy());
            Some(dump)
        }
        Err(e) => {
            tracing::warn!("no se pudo escribir en el archivo de volcado de audio: {e}");
            None
        }
    }
}

impl ReceiverState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        codec: AudioCodec,
        request: &AudioRequest,
        frames_per_packet: u32,
        control_remote: SocketAddr,
        cipher: AudioCipher,
        packet_tx: SyncSender<Packet>,
        free_rx: Receiver<Vec<u8>>,
        shared: Arc<RenderShared>,
    ) -> Self {
        let per_ms = |ms: u32| {
            let samples = u64::from(ms) * u64::from(request.sample_rate) / 1000;
            (samples.div_ceil(u64::from(frames_per_packet.max(1)))) as usize
        };
        let mut jitter = JitterBuffer::new(JITTER_CAPACITY, PACKET_MAX);
        jitter.set_skip_after(per_ms(SKIP_AFTER_MS).clamp(4, JITTER_CAPACITY / 2) as u16);
        Self {
            codec,
            cipher,
            jitter,
            packet_tx,
            free_rx,
            spare: None,
            shared,
            prebuffer_packets: per_ms(PREBUFFER_MS).max(2),
            playing: false,
            resend_target: (request.control_port != 0).then_some(control_remote),
            control_sequence: 0,
            last_resend: None,
            dropped: 0,
            received: 0,
            syncs: 0,
            dump: open_dump(request),
        }
    }

    fn on_data(&mut self, packet: &mut [u8]) {
        let Some(header) = Self::parse_data(packet) else {
            return;
        };
        self.received += 1;
        self.accept(header, packet);
    }

    fn parse_data(packet: &[u8]) -> Option<RtpHeader> {
        if packet.len() <= RtpHeader::LEN {
            return None;
        }
        RtpHeader::parse(packet).ok()
    }

    fn accept(&mut self, header: RtpHeader, packet: &mut [u8]) {
        match self.codec {
            AudioCodec::Alac if packet.len() == 44 => return,
            _ => {}
        }
        let payload = &mut packet[RtpHeader::LEN..];
        if payload.len() != NO_DATA_MARKER.len() {
            self.cipher.decrypt_in_place(payload);
        }
        if let Some(dump) = self.dump.as_mut()
            && dump.record(payload).is_err()
        {
            self.dump = None;
        }
        if self
            .jitter
            .insert(header.sequence, header.timestamp, payload)
            .is_err()
        {
            self.dropped += 1;
        }
    }

    fn on_control(&mut self, packet: &mut [u8], from: SocketAddr) {
        if packet.len() < 8 {
            return;
        }
        match packet[1] & 0x7F {
            0x56 if packet.len() >= 4 + RtpHeader::LEN => {
                let inner = &mut packet[4..];
                if let Ok(header) = RtpHeader::parse(inner) {
                    if inner.len() > RtpHeader::LEN {
                        self.accept(header, inner);
                    }
                }
            }
            0x54 if packet.len() >= 20 => {
                self.syncs += 1;
                if self.syncs == 1 {
                    tracing::debug!(
                        rtp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
                        "primer paquete de sincronización de audio"
                    );
                }
                if self.resend_target.is_some() {
                    self.resend_target = Some(from);
                }
            }
            other => tracing::debug!(
                kind = other,
                len = packet.len(),
                "paquete de control no reconocido"
            ),
        }
    }

    fn on_flush(&mut self, next: Option<u16>) {
        self.jitter.flush(next);
        self.playing = false;
        self.last_resend = None;
        self.shared.flush_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn pump(&mut self) {
        if !self.playing {
            if self.jitter.span() < self.prebuffer_packets {
                return;
            }
            self.playing = true;
        }
        loop {
            let mut buf = match self.spare.take() {
                Some(buf) => buf,
                None => match self.free_rx.try_recv() {
                    Ok(buf) => buf,
                    Err(_) => break,
                },
            };
            let Some((timestamp, payload)) = self.jitter.pop() else {
                self.spare = Some(buf);
                break;
            };
            if payload == NO_DATA_MARKER {
                self.spare = Some(buf);
                continue;
            }
            buf.clear();
            buf.extend_from_slice(payload);
            if self.packet_tx.try_send(Packet { buf, timestamp }).is_err() {
                self.dropped += 1;
                break;
            }
        }
    }
}

async fn send_resend_if_needed(state: &mut ReceiverState, control: &UdpSocket) {
    let Some(target) = state.resend_target else {
        return;
    };
    let Some((first, count)) = state.jitter.missing() else {
        return;
    };
    let now = Instant::now();
    if let Some((last_first, at)) = state.last_resend {
        if last_first == first && now.duration_since(at) < RESEND_INTERVAL {
            return;
        }
    }
    let mut out = [0u8; rtp::RESEND_REQUEST_LEN];
    rtp::encode_resend_request(state.control_sequence, first, count, &mut out);
    state.control_sequence = state.control_sequence.wrapping_add(1);
    state.last_resend = Some((first, now));
    if let Err(e) = control.send_to(&out, target).await {
        tracing::debug!("no se pudo enviar la petición de reenvío: {e}");
    }
}

async fn receive_loop(
    mut state: ReceiverState,
    data: UdpSocket,
    control: UdpSocket,
    mut commands: async_mpsc::UnboundedReceiver<Command>,
    cancel: CancellationToken,
) {
    let mut data_buf = vec![0u8; PACKET_MAX];
    let mut control_buf = vec![0u8; PACKET_MAX];
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            command = commands.recv() => match command {
                Some(Command::Flush(next)) => state.on_flush(next),
                None => break,
            },
            received = data.recv(&mut data_buf) => match received {
                Ok(len) => {
                    state.on_data(&mut data_buf[..len]);
                    state.pump();
                    send_resend_if_needed(&mut state, &control).await;
                }
                Err(e) => tracing::debug!("error al recibir audio: {e}"),
            },
            received = control.recv_from(&mut control_buf) => match received {
                Ok((len, from)) => {
                    state.on_control(&mut control_buf[..len], from);
                    state.pump();
                }
                Err(e) => tracing::debug!("error al recibir control de audio: {e}"),
            },
            _ = tick.tick() => {
                if let Some(dump) = state.dump.as_mut() {
                    let _ = dump.flush();
                }
                state.pump();
                send_resend_if_needed(&mut state, &control).await;
            }
        }
    }
    tracing::debug!(
        received = state.received,
        dropped = state.dropped,
        skipped = state.jitter.skipped(),
        "recepción de audio detenida"
    );
}

// Output options read from the live receiver settings.
#[derive(PartialEq)]
struct Output {
    device: Option<String>,
    delay_ms: i32,
}

impl Output {
    fn read(live: &Live) -> Self {
        let config = live.get();
        Self { device: config.audio_output, delay_ms: config.audio_delay_ms }
    }
}

fn delay_frames(delay_ms: i32) -> i64 {
    i64::from(delay_ms) * i64::from(rustplay_core::media::audio::SAMPLE_RATE) / 1000
}

fn open_renderer(latency_ms: u32, device: Option<&str>) -> Option<Renderer> {
    match Renderer::start_on(latency_ms, device) {
        Ok(renderer) => Some(renderer),
        Err(e) => {
            tracing::warn!("no se pudo iniciar la salida de audio: {e}");
            None
        }
    }
}

// Lip-sync offset relative to the plain "play as soon as buffered" schedule: positive values
// queue silence ahead of the audio, negative ones drop audio so it plays earlier (bounded by
// what is buffered). Applied again after every flush and whenever the setting changes.
struct DelayLine {
    target: i64,
    applied: i64,
}

impl DelayLine {
    fn reset(&mut self) {
        self.applied = 0;
    }

    // Frames of silence to queue now (positive) or decoded frames still to drop.
    fn pending(&self) -> i64 {
        self.target - self.applied
    }
}

// Pushes all of `pcm`, waiting for room; gives up on stop or flush.
fn push_all(renderer: &mut Renderer, pcm: &[i16], channels: usize, shared: &RenderShared, generation: u32) {
    let mut offset = 0;
    while offset < pcm.len() {
        let accepted = renderer.push(&pcm[offset..]);
        if accepted == 0 {
            if shared.stop.load(Ordering::Acquire) || shared.flush_generation.load(Ordering::Acquire) != generation {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        } else {
            offset += accepted * channels;
        }
    }
}

fn render_thread(
    format: AudioFormat,
    latency_ms: u32,
    packets: Receiver<Packet>,
    free: SyncSender<Vec<u8>>,
    shared: Arc<RenderShared>,
    live: &Live,
) {
    let capture = &*live.capture;
    let spec = rustplay_core::media::recorder::AudioSpec { sample_rate: format.sample_rate, channels: format.channels };
    capture.set_audio(Some(spec));
    let mut decoder = match create_decoder(&format) {
        Ok(decoder) => Some(decoder),
        Err(rustplay_core::Error::NotImplemented(_)) if format.codec == AudioCodec::AacEld => {
            tracing::info!("el audio AAC-ELD de la duplicación aún no está disponible: se descarta");
            None
        }
        Err(e) => {
            tracing::warn!(
                "no se pudo crear el decodificador de audio {:?}: {e}",
                format.codec
            );
            None
        }
    };
    let mut output = Output::read(live);
    let mut live_generation = live.generation();
    let mut renderer = open_renderer(latency_ms, output.device.as_deref());
    let mut delay = DelayLine { target: delay_frames(output.delay_ms), applied: 0 };
    if output.delay_ms != 0 {
        tracing::info!("retraso del audio recibido: {} ms", output.delay_ms);
    }
    let channels = usize::from(format.channels);
    let mut pcm = vec![0i16; 4096 * channels];
    let silence = vec![0i16; 1024 * channels];
    let mut generation = shared.flush_generation.load(Ordering::Acquire);
    let mut volume_bits = u32::MAX;
    let mut decode_errors = 0u64;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let current = shared.flush_generation.load(Ordering::Acquire);
        if current != generation {
            generation = current;
            while let Ok(old) = packets.try_recv() {
                let _ = free.try_send(old.buf);
            }
            if let Some(r) = renderer.as_mut() {
                r.flush();
            }
            delay.reset();
        }
        let now = live.generation();
        if now != live_generation {
            live_generation = now;
            let next = Output::read(live);
            if next != output {
                if next.device != output.device {
                    tracing::info!("cambiando el altavoz del audio recibido");
                    drop(renderer.take());
                    renderer = open_renderer(latency_ms, next.device.as_deref());
                    volume_bits = u32::MAX;
                    delay.reset();
                }
                if next.delay_ms != output.delay_ms {
                    tracing::info!("retraso del audio recibido: {} ms", next.delay_ms);
                }
                delay.target = delay_frames(next.delay_ms);
                output = next;
            }
        }
        let bits = if shared.muted.load(Ordering::Acquire) || shared.user_muted.load(Ordering::Acquire) {
            0f32.to_bits()
        } else {
            shared.volume_bits.load(Ordering::Relaxed)
        };
        if bits != volume_bits {
            volume_bits = bits;
            if let Some(r) = renderer.as_mut() {
                r.set_volume(f32::from_bits(bits));
            }
        }
        let packet = match packets.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let (Some(decoder), Some(renderer)) = (decoder.as_mut(), renderer.as_mut()) {
            match decoder.decode(&packet.buf, &mut pcm) {
                Ok(frames) => {
                    let written = (frames * channels).min(pcm.len());
                    capture.push_audio(spec, &pcm[..written]);
                    let mut start = 0;
                    let pending = delay.pending();
                    if pending > 0 {
                        let mut left = pending as usize * channels;
                        while left > 0 {
                            let n = left.min(silence.len());
                            push_all(renderer, &silence[..n], channels, &shared, generation);
                            left -= n;
                        }
                        delay.applied = delay.target;
                    } else if pending < 0 {
                        let drop = ((-pending) as usize).min(written / channels);
                        start = drop * channels;
                        delay.applied -= drop as i64;
                    }
                    push_all(renderer, &pcm[start..written], channels, &shared, generation);
                }
                Err(e) => {
                    decode_errors += 1;
                    if decode_errors == 1 || decode_errors % 500 == 0 {
                        tracing::warn!("error al decodificar audio ({decode_errors}): {e}");
                    }
                }
            }
        }
        let _ = packet.timestamp;
        let _ = free.try_send(packet.buf);
    }
    capture.set_audio(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(latency_min: Option<u32>) -> AudioRequest {
        AudioRequest {
            ct: 2,
            spf: 352,
            sample_rate: 44_100,
            control_port: 1,
            audio_format: 0,
            latency_min,
        }
    }

    fn state(codec_ct: u8) -> (ReceiverState, Receiver<Packet>, SyncSender<Vec<u8>>) {
        let mut req = request(None);
        req.ct = codec_ct;
        let (packet_tx, packet_rx) = mpsc::sync_channel(POOL_SIZE);
        let (free_tx, free_rx) = mpsc::sync_channel(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            free_tx.send(Vec::with_capacity(PACKET_MAX)).unwrap();
        }
        let shared = Arc::new(RenderShared {
            stop: AtomicBool::new(false),
            flush_generation: AtomicU32::new(0),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            muted: Arc::new(AtomicBool::new(false)),
            user_muted: Arc::new(AtomicBool::new(false)),
        });
        let state = ReceiverState::new(
            req.codec(),
            &req,
            req.frames_per_packet(),
            "127.0.0.1:9".parse().unwrap(),
            AudioCipher::new([1; 16], [2; 16]),
            packet_tx,
            free_rx,
            shared,
        );
        (state, packet_rx, free_tx)
    }

    fn rtp_packet(seq: u16, payload_len: usize) -> Vec<u8> {
        let mut raw = [0u8; RtpHeader::LEN];
        RtpHeader {
            marker: false,
            payload_type: 96,
            sequence: seq,
            timestamp: u32::from(seq) * 352,
            ssrc: 0,
        }
        .write(&mut raw);
        let mut packet = raw.to_vec();
        packet.extend(std::iter::repeat_n(seq as u8, payload_len));
        packet
    }

    #[test]
    fn pipeline_prebuffers_reorders_and_requests_resends() {
        let (mut st, rx, _free) = state(2);
        for seq in 0..5u16 {
            st.on_data(&mut rtp_packet(seq, 48));
        }
        st.pump();
        assert!(rx.try_recv().is_err(), "debe esperar al prebúfer");
        st.on_data(&mut rtp_packet(6, 48));
        for seq in 7..20u16 {
            st.on_data(&mut rtp_packet(seq, 48));
        }
        st.pump();
        let got: Vec<u32> = rx.try_iter().map(|p| p.timestamp / 352).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
        assert_eq!(st.jitter.missing(), Some((5, 1)));
        let mut resent = vec![0x80, 0xD6, 0, 0];
        resent.extend(rtp_packet(5, 48));
        st.on_control(&mut resent, "127.0.0.1:9".parse().unwrap());
        st.pump();
        let got: Vec<u32> = rx.try_iter().map(|p| p.timestamp / 352).collect();
        assert_eq!(got, (5..20).collect::<Vec<_>>());
    }

    #[test]
    fn pipeline_ignores_alac_format_packets_and_buffers_aac_eld() {
        let (mut st, rx, _free) = state(2);
        let mut format_packet = rtp_packet(0, 32);
        assert_eq!(format_packet.len(), 44);
        st.on_data(&mut format_packet);
        assert_eq!(st.jitter.span(), 0);
        assert!(rx.try_recv().is_err());
        let (mut eld, _rx2, _f) = state(8);
        eld.on_data(&mut rtp_packet(1, 100));
        assert_eq!(eld.jitter.span(), 1);
    }

    #[test]
    fn dump_records_have_the_documented_layout() {
        let mut out = Vec::new();
        {
            let mut dump = AudioDump::new(&mut out, 8, 480, 44_100).unwrap();
            dump.record(&[1, 2, 3]).unwrap();
            dump.record(&[]).unwrap();
            dump.flush().unwrap();
        }
        let mut expected = vec![0, 0, 0, 0, 8];
        expected.extend_from_slice(&480u32.to_le_bytes());
        expected.extend_from_slice(&44_100u32.to_le_bytes());
        expected.extend_from_slice(&[3, 0, 0, 0, 1, 2, 3, 0, 0, 0, 0]);
        assert_eq!(out, expected);
    }

    #[test]
    fn flush_discards_and_bumps_generation() {
        let (mut st, rx, _free) = state(2);
        for seq in 0..20u16 {
            st.on_data(&mut rtp_packet(seq, 48));
        }
        st.on_flush(Some(100));
        assert_eq!(st.shared.flush_generation.load(Ordering::Acquire), 1);
        st.pump();
        assert!(rx.try_recv().is_err());
        for seq in 100..120u16 {
            st.on_data(&mut rtp_packet(seq, 48));
        }
        st.pump();
        assert_eq!(rx.try_iter().count(), 20);
    }

    #[test]
    fn pool_exhaustion_applies_backpressure_without_loss() {
        let (mut st, rx, free) = state(2);
        for seq in 0..300u16 {
            st.on_data(&mut rtp_packet(seq, 48));
        }
        st.pump();
        assert_eq!(rx.try_iter().count(), POOL_SIZE);
        for _ in 0..4 {
            free.send(Vec::new()).unwrap();
        }
        st.pump();
        assert_eq!(rx.try_iter().count(), 4);
    }

    #[test]
    fn latency_is_clamped() {
        assert_eq!(latency_ms_for(&request(None)), DEFAULT_LATENCY_MS);
        assert_eq!(latency_ms_for(&request(Some(441))), DEFAULT_LATENCY_MS);
        assert_eq!(latency_ms_for(&request(Some(22_050))), 500);
        assert_eq!(latency_ms_for(&request(Some(11_025))), 250);
    }
}

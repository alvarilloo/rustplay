use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::Result;

pub const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;
pub const REQUEST_LEN: usize = 32;

const FRAC_SCALE: f64 = 4_294_967_296.0;
const DATA_COUNT: usize = 8;
const MIN_DELAY: f64 = 0.001;
const MAX_DIST: f64 = 1.5;
const ALPHA: f64 = 0.05;
const BURST_INTERVAL: Duration = Duration::from_millis(250);
const STEADY_INTERVAL: Duration = Duration::from_secs(3);
const REPLY_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTimestamp(pub u64);

impl NtpTimestamp {
    pub fn now() -> Self {
        let since_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self::from_unix(since_unix)
    }

    pub fn from_unix(since_unix: Duration) -> Self {
        let secs = since_unix.as_secs() + NTP_EPOCH_OFFSET;
        let frac = (u128::from(since_unix.subsec_nanos()) << 32) / 1_000_000_000;
        Self(((secs & 0xFFFF_FFFF) << 32) | frac as u64)
    }

    pub fn from_secs_frac(secs: u32, frac: u32) -> Self {
        Self(((secs as u64) << 32) | frac as u64)
    }

    pub fn secs(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub fn frac(self) -> u32 {
        self.0 as u32
    }

    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub fn from_be_slice(buf: &[u8]) -> Self {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&buf[..8]);
        Self(u64::from_be_bytes(raw))
    }

    pub fn seconds_since(self, earlier: Self) -> f64 {
        (self.0.wrapping_sub(earlier.0) as i64) as f64 / FRAC_SCALE
    }

    pub fn offset_by(self, seconds: f64) -> Self {
        Self(self.0.wrapping_add((seconds * FRAC_SCALE) as i64 as u64))
    }
}

pub fn encode_request(
    send_time: NtpTimestamp,
    reference: NtpTimestamp,
    previous_recv: NtpTimestamp,
    out: &mut [u8; REQUEST_LEN],
) {
    out.fill(0);
    out[0] = 0x80;
    out[1] = 0xD2;
    out[3] = 0x07;
    out[8..16].copy_from_slice(&reference.to_be_bytes());
    out[16..24].copy_from_slice(&previous_recv.to_be_bytes());
    out[24..32].copy_from_slice(&send_time.to_be_bytes());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingReply {
    pub t1: NtpTimestamp,
    pub t2: NtpTimestamp,
    pub t3: NtpTimestamp,
}

pub fn decode_reply(buf: &[u8]) -> Option<TimingReply> {
    if buf.len() < REQUEST_LEN || buf[0] >> 6 != 2 || buf[1] & 0x7F != 0x53 {
        return None;
    }
    Some(TimingReply {
        t1: NtpTimestamp::from_be_slice(&buf[8..16]),
        t2: NtpTimestamp::from_be_slice(&buf[16..24]),
        t3: NtpTimestamp::from_be_slice(&buf[24..32]),
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimingSample {
    pub offset: f64,
    pub delay: f64,
}

pub fn compute_sample(reply: &TimingReply, t4: NtpTimestamp) -> TimingSample {
    let client_to_server = reply.t2.seconds_since(reply.t1);
    let server_to_client = reply.t3.seconds_since(t4);
    let total = t4.seconds_since(reply.t1);
    let processing = reply.t3.seconds_since(reply.t2);
    TimingSample {
        offset: (client_to_server + server_to_client) / 2.0,
        delay: (total - processing).max(MIN_DELAY),
    }
}

#[derive(Debug, Default)]
pub struct OffsetFilter {
    samples: [Option<TimingSample>; DATA_COUNT],
    index: usize,
    smoothed: Option<f64>,
}

impl OffsetFilter {
    pub fn push(&mut self, sample: TimingSample) -> (f64, bool) {
        self.samples[self.index] = Some(sample);
        self.index = (self.index + 1) % DATA_COUNT;
        let best = self
            .samples
            .iter()
            .flatten()
            .min_by(|a, b| a.delay.total_cmp(&b.delay))
            .copied()
            .unwrap_or(sample);
        if best.delay * 0.5 > MAX_DIST {
            return (best.offset, false);
        }
        let offset = match self.smoothed {
            Some(previous) => (1.0 - ALPHA) * previous + ALPHA * best.offset,
            None => best.offset,
        };
        self.smoothed = Some(offset);
        (offset, true)
    }
}

struct Shared {
    offset: AtomicI64,
    synced: AtomicBool,
    have_offset: AtomicBool,
}

pub struct NtpClock {
    pub peer: SocketAddr,
    local_port: u16,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl NtpClock {
    pub async fn bind(peer: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind((crate::net::listen_addr(peer.ip()), 0)).await?;
        let local_port = socket.local_addr()?.port();
        let shared = Arc::new(Shared {
            offset: AtomicI64::new(0),
            synced: AtomicBool::new(false),
            have_offset: AtomicBool::new(false),
        });
        let task = tokio::spawn(run(socket, peer, Arc::clone(&shared)));
        Ok(Self {
            peer,
            local_port,
            shared,
            task,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn offset_seconds(&self) -> Option<f64> {
        self.shared
            .have_offset
            .load(Ordering::Acquire)
            .then(|| self.shared.offset.load(Ordering::Relaxed) as f64 / FRAC_SCALE)
    }

    pub fn offset(&self) -> Option<Duration> {
        self.offset_seconds()
            .map(|s| Duration::from_secs_f64(s.abs()))
    }

    pub fn is_synced(&self) -> bool {
        self.shared.synced.load(Ordering::Relaxed)
    }

    pub fn remote_to_local(&self, remote: NtpTimestamp) -> NtpTimestamp {
        match self.offset_seconds() {
            Some(offset) => remote.offset_by(-offset),
            None => remote,
        }
    }
}

impl Drop for NtpClock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(socket: UdpSocket, peer: SocketAddr, shared: Arc<Shared>) {
    let mut filter = OffsetFilter::default();
    let mut request = [0u8; REQUEST_LEN];
    let mut response = [0u8; 128];
    let mut sent = [NtpTimestamp(0); DATA_COUNT];
    let mut sent_index = 0usize;
    let mut reference = NtpTimestamp(0);
    let mut previous_recv = NtpTimestamp(0);
    let mut last_t3 = NtpTimestamp(0);
    let mut count = 0usize;
    loop {
        while socket.try_recv_from(&mut response).is_ok() {}
        let t1 = NtpTimestamp::now();
        encode_request(t1, reference, previous_recv, &mut request);
        match socket.send_to(&request, peer).await {
            Ok(_) => {
                sent[sent_index] = t1;
                sent_index = (sent_index + 1) % DATA_COUNT;
                count += 1;
                let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
                while let Ok(received) =
                    tokio::time::timeout_at(deadline, socket.recv_from(&mut response)).await
                {
                    let Ok((len, from)) = received else { continue };
                    let t4 = NtpTimestamp::now();
                    if from.ip() != peer.ip() {
                        continue;
                    }
                    let Some(reply) = decode_reply(&response[..len]) else {
                        continue;
                    };
                    if !sent.contains(&reply.t1) || reply.t3 == last_t3 {
                        tracing::debug!(
                            "respuesta de tiempo NTP descartada (referencia no válida o duplicada)"
                        );
                        continue;
                    }
                    last_t3 = reply.t3;
                    reference = reply.t3;
                    previous_recv = t4;
                    let sample = compute_sample(&reply, t4);
                    let (offset, synced) = filter.push(sample);
                    shared
                        .offset
                        .store((offset * FRAC_SCALE) as i64, Ordering::Relaxed);
                    shared.synced.store(synced, Ordering::Relaxed);
                    shared.have_offset.store(true, Ordering::Release);
                    tracing::debug!(offset, delay = sample.delay, synced, "sincronización NTP");
                    break;
                }
            }
            Err(e) => tracing::debug!("no se pudo enviar la petición NTP: {e}"),
        }
        let wait = if count > DATA_COUNT {
            STEADY_INTERVAL
        } else {
            BURST_INTERVAL
        };
        tokio::time::sleep(wait).await;
    }
}

impl From<NtpTimestamp> for Duration {
    fn from(value: NtpTimestamp) -> Self {
        let secs = u64::from(value.secs()).saturating_sub(NTP_EPOCH_OFFSET);
        let nanos = ((u128::from(value.frac()) * 1_000_000_000) >> 32) as u32;
        Duration::new(secs, nanos)
    }
}

pub fn ntp_now() -> NtpTimestamp {
    NtpTimestamp::now()
}

pub fn build_reply(
    request: &[u8],
    received: NtpTimestamp,
    transmit: NtpTimestamp,
    out: &mut [u8; REQUEST_LEN],
) -> bool {
    if request.len() < REQUEST_LEN || request[0] >> 6 != 2 || request[1] & 0x7F != 0x52 {
        return false;
    }
    out[..8].copy_from_slice(&request[..8]);
    out[1] = 0xD3;
    out[8..16].copy_from_slice(&request[24..32]);
    out[16..24].copy_from_slice(&received.to_be_bytes());
    out[24..32].copy_from_slice(&transmit.to_be_bytes());
    true
}

pub struct NtpResponder {
    local_port: u16,
    socket: std::sync::Arc<UdpSocket>,
    task: JoinHandle<()>,
    probes: Option<JoinHandle<()>>,
}

impl NtpResponder {
    pub async fn bind(peer: IpAddr) -> Result<Self> {
        let socket = std::sync::Arc::new(UdpSocket::bind((crate::net::listen_addr(peer), 0)).await?);
        let local_port = socket.local_addr()?.port();
        let task = tokio::spawn(respond(std::sync::Arc::clone(&socket)));
        Ok(Self {
            local_port,
            socket,
            task,
            probes: None,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    // Opens the host firewall for the receiver's timing requests, as doubletake does.
    pub fn probe(&mut self, receiver: SocketAddr) {
        if self.probes.is_some() {
            return;
        }
        let socket = std::sync::Arc::clone(&self.socket);
        self.probes = Some(tokio::spawn(async move {
            for sequence in 1u16..=3 {
                let mut request = [0u8; REQUEST_LEN];
                request[0] = 0x80;
                request[1] = 0xD2;
                request[2..4].copy_from_slice(&sequence.to_be_bytes());
                request[24..32].copy_from_slice(&ntp_now().to_be_bytes());
                if let Err(e) = socket.send_to(&request, receiver).await {
                    tracing::debug!("no se pudo enviar la sonda NTP a {receiver}: {e}");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }));
    }
}

impl Drop for NtpResponder {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(probes) = &self.probes {
            probes.abort();
        }
    }
}

async fn respond(socket: std::sync::Arc<UdpSocket>) {
    let mut buf = [0u8; 128];
    let mut reply = [0u8; REQUEST_LEN];
    let mut requests = 0u64;
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                tracing::debug!("error de lectura en el respondedor NTP: {e}");
                if e.kind() == std::io::ErrorKind::ConnectionReset {
                    continue;
                }
                return;
            }
        };
        let received = ntp_now();
        if !build_reply(&buf[..len], received, ntp_now(), &mut reply) {
            continue;
        }
        requests += 1;
        if requests <= 3 || requests % 50 == 0 {
            tracing::debug!(%from, requests, "petición de tiempo NTP recibida");
        }
        if let Err(e) = socket.send_to(&reply, from).await {
            tracing::debug!("no se pudo enviar la respuesta NTP: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: u32, frac: u32) -> NtpTimestamp {
        NtpTimestamp::from_secs_frac(secs, frac)
    }

    #[test]
    fn from_unix_maps_epoch() {
        let t = NtpTimestamp::from_unix(Duration::new(10, 500_000_000));
        assert_eq!(u64::from(t.secs()), 10 + NTP_EPOCH_OFFSET);
        assert_eq!(t.frac(), 0x8000_0000);
        assert_eq!(Duration::from(t), Duration::new(10, 500_000_000));
    }

    #[test]
    fn now_is_recent() {
        let now = NtpTimestamp::now();
        assert!(u64::from(now.secs()) > NTP_EPOCH_OFFSET + 1_700_000_000);
    }

    #[test]
    fn request_layout() {
        let mut out = [0u8; REQUEST_LEN];
        encode_request(ts(5, 6), ts(1, 2), ts(3, 4), &mut out);
        assert_eq!(&out[..4], &[0x80, 0xD2, 0x00, 0x07]);
        assert_eq!(&out[4..8], &[0; 4]);
        assert_eq!(&out[8..16], &ts(1, 2).to_be_bytes());
        assert_eq!(&out[16..24], &ts(3, 4).to_be_bytes());
        assert_eq!(&out[24..32], &ts(5, 6).to_be_bytes());
    }

    #[test]
    fn reply_decoding() {
        let mut buf = [0u8; REQUEST_LEN];
        buf[0] = 0x80;
        buf[1] = 0xD3;
        buf[8..16].copy_from_slice(&ts(10, 1).to_be_bytes());
        buf[16..24].copy_from_slice(&ts(20, 2).to_be_bytes());
        buf[24..32].copy_from_slice(&ts(21, 3).to_be_bytes());
        let reply = decode_reply(&buf).unwrap();
        assert_eq!(reply.t1, ts(10, 1));
        assert_eq!(reply.t2, ts(20, 2));
        assert_eq!(reply.t3, ts(21, 3));
        buf[1] = 0xD2;
        assert!(decode_reply(&buf).is_none());
        assert!(decode_reply(&buf[..16]).is_none());
    }

    #[test]
    fn offset_and_delay_math() {
        let reply = TimingReply {
            t1: ts(100, 0),
            t2: ts(1000, 0x4000_0000),
            t3: ts(1000, 0x8000_0000),
        };
        let t4 = ts(100, 0x8000_0000);
        let sample = compute_sample(&reply, t4);
        assert!((sample.delay - 0.25).abs() < 1e-6);
        assert!((sample.offset - 900.125).abs() < 1e-6);
    }

    #[test]
    fn negative_offset() {
        let reply = TimingReply {
            t1: ts(1000, 0),
            t2: ts(100, 0),
            t3: ts(100, 0),
        };
        let sample = compute_sample(&reply, ts(1000, 0x1000_0000));
        assert!(sample.offset < -899.0 && sample.offset > -901.0);
        assert!((sample.delay - 0.0625).abs() < 1e-6);
    }

    #[test]
    fn filter_prefers_low_delay_and_smooths() {
        let mut filter = OffsetFilter::default();
        let (first, synced) = filter.push(TimingSample {
            offset: 1.0,
            delay: 0.01,
        });
        assert!(synced);
        assert_eq!(first, 1.0);
        let (second, _) = filter.push(TimingSample {
            offset: 5.0,
            delay: 0.5,
        });
        assert_eq!(second, 1.0);
        let (third, _) = filter.push(TimingSample {
            offset: 2.0,
            delay: 0.001,
        });
        assert!((third - (0.95 * 1.0 + 0.05 * 2.0)).abs() < 1e-12);
    }

    #[test]
    fn remote_to_local_shifts_by_offset() {
        let remote = ts(1000, 0);
        let local = remote.offset_by(-900.5);
        assert!((remote.seconds_since(local) - 900.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn clock_syncs_against_a_fake_peer() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((len, from)) = peer.recv_from(&mut buf).await else {
                    return;
                };
                if len < REQUEST_LEN {
                    continue;
                }
                let skew = NtpTimestamp::now().offset_by(100.0);
                let mut reply = [0u8; REQUEST_LEN];
                reply[0] = 0x80;
                reply[1] = 0xD3;
                reply[8..16].copy_from_slice(&buf[24..32]);
                reply[16..24].copy_from_slice(&skew.to_be_bytes());
                reply[24..32].copy_from_slice(&skew.to_be_bytes());
                let _ = peer.send_to(&reply, from).await;
            }
        });
        let clock = NtpClock::bind(peer_addr).await.unwrap();
        assert_ne!(clock.local_port(), 0);
        let mut offset = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            offset = clock.offset_seconds();
            if offset.is_some() {
                break;
            }
        }
        let offset = offset.expect("sin sincronización");
        assert!((offset - 100.0).abs() < 0.1, "offset = {offset}");
        server.abort();
    }

    #[test]
    fn reply_mirrors_request() {
        let mut request = [0u8; REQUEST_LEN];
        encode_request(ts(5, 6), ts(1, 2), ts(3, 4), &mut request);
        let mut out = [0u8; REQUEST_LEN];
        assert!(build_reply(&request, ts(50, 1), ts(50, 2), &mut out));
        assert_eq!(&out[..8], &[0x80, 0xD3, 0x00, 0x07, 0, 0, 0, 0]);
        let reply = decode_reply(&out).unwrap();
        assert_eq!(reply.t1, ts(5, 6));
        assert_eq!(reply.t2, ts(50, 1));
        assert_eq!(reply.t3, ts(50, 2));
        assert!(!build_reply(
            &out,
            ts(0, 0),
            ts(0, 0),
            &mut [0u8; REQUEST_LEN]
        ));
        assert!(!build_reply(&request[..31], ts(0, 0), ts(0, 0), &mut out));
    }

    #[tokio::test]
    async fn clock_syncs_against_responder() {
        let responder = NtpResponder::bind(std::net::Ipv4Addr::LOCALHOST.into()).await.unwrap();
        assert_ne!(responder.local_port(), 0);
        let peer: SocketAddr = ([127, 0, 0, 1], responder.local_port()).into();
        let clock = NtpClock::bind(peer).await.unwrap();
        let mut offset = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            offset = clock.offset_seconds();
            if offset.is_some() {
                break;
            }
        }
        let offset = offset.expect("sin sincronización");
        assert!(offset.abs() < 0.005, "offset = {offset}");
        assert!(clock.is_synced());
    }

    #[tokio::test]
    async fn responder_stops_on_drop() {
        let responder = NtpResponder::bind(std::net::Ipv4Addr::LOCALHOST.into()).await.unwrap();
        let port = responder.local_port();
        drop(responder);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(UdpSocket::bind(("127.0.0.1", port)).await.is_ok());
    }
}

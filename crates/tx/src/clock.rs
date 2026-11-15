use std::net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustplay_core::ntp::{NTP_EPOCH_OFFSET, REQUEST_LEN, NtpTimestamp, build_reply};
use rustplay_core::{Error, Result};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

// doubletake's non-Linux boot clock: a monotonic clock relative to the start of the process.
pub fn origin() -> Instant {
    *ORIGIN.get_or_init(Instant::now)
}

pub fn boot_of(at: Instant) -> Duration {
    at.saturating_duration_since(origin())
}

pub fn boot_now() -> Duration {
    boot_of(Instant::now())
}

// 32.32 fixed point seconds.
pub fn compact(duration: Duration) -> u64 {
    let nanos = u128::from(duration.subsec_nanos());
    let fraction = (nanos << 32) / 1_000_000_000;
    (duration.as_secs() << 32) | fraction as u64
}

// Timing and audio sync packets carry the boot clock plus the NTP epoch; video headers carry it bare.
pub fn ntp_boot_timestamp() -> u64 {
    ntp_boot_timestamp_at(Instant::now())
}

pub fn ntp_boot_timestamp_at(at: Instant) -> u64 {
    compact(boot_of(at)).wrapping_add(NTP_EPOCH_OFFSET << 32)
}

pub fn video_timestamp(at: Instant, bias: Duration) -> u64 {
    compact(boot_of(at) + bias)
}

// Binds `count` consecutive UDP ports (timing, control, data). With `range` the search is
// limited to it; otherwise the first port is chosen by the OS.
pub fn allocate_consecutive_udp(count: usize, range: Option<(u16, u16)>, peer: IpAddr) -> Result<Vec<StdUdpSocket>> {
    let local = rustplay_core::net::listen_addr(peer);
    allocate_with(count, range, |port| StdUdpSocket::bind(SocketAddr::from((local, port))))
}

fn allocate_with(
    count: usize,
    range: Option<(u16, u16)>,
    bind: impl Fn(u16) -> std::io::Result<StdUdpSocket>,
) -> Result<Vec<StdUdpSocket>> {
    let attempt = |base: u16| -> Option<Vec<StdUdpSocket>> {
        let first = bind(base).ok()?;
        let actual = first.local_addr().ok()?.port();
        let mut sockets = vec![first];
        for offset in 1..count {
            let port = actual.checked_add(u16::try_from(offset).ok()?)?;
            sockets.push(bind(port).ok()?);
        }
        Some(sockets)
    };
    match range {
        None => (0..20)
            .find_map(|_| attempt(0))
            .ok_or_else(|| Error::protocol(format!("no se pudieron reservar {count} puertos UDP consecutivos"))),
        Some((min, max)) => {
            if min == 0 || min > max || usize::from(max - min) + 1 < count {
                return Err(Error::protocol(format!("intervalo de puertos UDP no válido: {min}-{max}")));
            }
            (min..=max - (count as u16 - 1))
                .find_map(attempt)
                .ok_or_else(|| Error::protocol(format!("no hay {count} puertos UDP consecutivos libres en {min}-{max}")))
        }
    }
}

pub fn into_tokio(socket: StdUdpSocket) -> Result<UdpSocket> {
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket)?)
}

// Answers the receiver's timing requests on the advertised timing port and, like doubletake,
// pokes the receiver's own timing port a few times so the host firewall lets its replies in.
pub struct TimingService {
    port: u16,
    socket: Arc<UdpSocket>,
    task: JoinHandle<()>,
    probes: Option<JoinHandle<()>>,
}

impl TimingService {
    pub fn start(socket: StdUdpSocket) -> Result<Self> {
        let socket = Arc::new(into_tokio(socket)?);
        let port = socket.local_addr()?.port();
        let task = tokio::spawn(respond(Arc::clone(&socket)));
        Ok(Self { port, socket, task, probes: None })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn probe(&mut self, receiver: SocketAddr) {
        if self.probes.is_some() {
            return;
        }
        let socket = Arc::clone(&self.socket);
        self.probes = Some(tokio::spawn(async move {
            for sequence in 1u16..=3 {
                let mut request = [0u8; REQUEST_LEN];
                request[0] = 0x80;
                request[1] = 0xD2;
                request[2..4].copy_from_slice(&sequence.to_be_bytes());
                request[24..32].copy_from_slice(&ntp_boot_timestamp().to_be_bytes());
                if let Err(e) = socket.send_to(&request, receiver).await {
                    tracing::debug!("no se pudo enviar la sonda de tiempo a {receiver}: {e}");
                    return;
                }
                if sequence < 3 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }));
    }
}

impl Drop for TimingService {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(probes) = &self.probes {
            probes.abort();
        }
    }
}

async fn respond(socket: Arc<UdpSocket>) {
    let mut buf = [0u8; 128];
    let mut reply = [0u8; REQUEST_LEN];
    let mut requests = 0u64;
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                tracing::debug!("error de lectura en el respondedor de tiempo: {e}");
                return;
            }
        };
        let now = NtpTimestamp(ntp_boot_timestamp());
        if !build_reply(&buf[..len], now, now, &mut reply) {
            continue;
        }
        requests += 1;
        if requests <= 3 || requests % 5 == 0 {
            tracing::debug!(%from, requests, "petición de tiempo recibida");
        }
        if let Err(e) = socket.send_to(&reply, from).await {
            tracing::debug!("no se pudo responder a la petición de tiempo: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn compact_is_32_32_fixed_point() {
        assert_eq!(compact(Duration::from_secs(3)), 3 << 32);
        assert_eq!(compact(Duration::from_millis(500)), 1 << 31);
        assert_eq!(compact(Duration::from_millis(1500)), (1 << 32) | (1 << 31));
    }

    #[test]
    fn video_timestamp_is_bare_boot_time_plus_bias() {
        let at = origin() + Duration::from_secs(2);
        assert_eq!(video_timestamp(at, Duration::from_millis(75)), compact(Duration::from_millis(2075)));
        assert_eq!(ntp_boot_timestamp_at(at) >> 32, 2 + NTP_EPOCH_OFFSET);
    }

    #[test]
    fn consecutive_ports_are_adjacent_and_range_is_honoured() {
        let sockets = allocate_consecutive_udp(3, None, Ipv4Addr::LOCALHOST.into()).unwrap();
        let ports: Vec<u16> = sockets.iter().map(|s| s.local_addr().unwrap().port()).collect();
        assert_eq!(ports[1], ports[0] + 1);
        assert_eq!(ports[2], ports[0] + 2);
        let base = ports[0];
        drop(sockets);
        let ranged = allocate_consecutive_udp(3, Some((base, base + 5)), Ipv4Addr::LOCALHOST.into()).unwrap();
        assert!((base..=base + 5).contains(&ranged[0].local_addr().unwrap().port()));
        assert!(allocate_consecutive_udp(3, Some((5000, 5001)), Ipv4Addr::LOCALHOST.into()).is_err());
        assert!(allocate_consecutive_udp(3, Some((0, 10)), Ipv4Addr::LOCALHOST.into()).is_err());
    }

    #[test]
    fn allocation_skips_a_busy_neighbour() {
        let busy = StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let sockets = allocate_consecutive_udp(2, Some((busy_port.saturating_sub(1).max(1), busy_port.saturating_add(200))), Ipv4Addr::LOCALHOST.into()).unwrap();
        let ports: Vec<u16> = sockets.iter().map(|s| s.local_addr().unwrap().port()).collect();
        assert!(!ports.contains(&busy_port));
        assert_eq!(ports[1], ports[0] + 1);
    }

    #[tokio::test]
    async fn responder_answers_requests_with_the_boot_clock_and_ignores_replies() {
        let service = TimingService::start(StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()).unwrap();
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, service.port()));
        let mut request = [0u8; REQUEST_LEN];
        request[0] = 0x80;
        request[1] = 0xD2;
        request[2..4].copy_from_slice(&9u16.to_be_bytes());
        request[24..32].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        let mut stray = request;
        stray[1] = 0xD3;
        client.send_to(&stray, target).await.unwrap();
        client.send_to(&request, target).await.unwrap();
        let mut buf = [0u8; 64];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(len, REQUEST_LEN);
        assert_eq!(&buf[..4], &[0x80, 0xD3, 0, 9]);
        assert_eq!(&buf[8..16], &0x1122_3344_5566_7788u64.to_be_bytes());
        let received = u64::from_be_bytes(buf[16..24].try_into().unwrap());
        assert!(received >> 32 >= NTP_EPOCH_OFFSET);
        assert!((received >> 32) - NTP_EPOCH_OFFSET < 3600);
        assert_eq!(&buf[16..24], &buf[24..32]);
    }

    #[tokio::test]
    async fn probes_send_three_timing_requests() {
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut service = TimingService::start(StdUdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()).unwrap();
        service.probe(receiver.local_addr().unwrap());
        for expected in 1u16..=3 {
            let mut buf = [0u8; 64];
            let (len, _) = tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(len, REQUEST_LEN);
            assert_eq!(buf[..2], [0x80, 0xD2]);
            assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), expected);
        }
    }
}

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

use crate::audio::{self, PendingCapture, Pipeline, PipelineParams};
use crate::conn::RtspConn;
use crate::video::{DataSender, MirrorStats, Unit};
use crate::video_thread::{Shared, VideoThread};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(5);
const MAX_QUEUED_AGE: Duration = Duration::from_millis(500);

pub struct AudioLaunch {
    pub pending: PendingCapture,
    pub params: PipelineParams,
}

pub struct MirrorPipeline {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    video: VideoThread,
    tasks: Vec<JoinHandle<()>>,
    closed: watch::Receiver<bool>,
    pub size: (u32, u32),
    pub encoder: String,
}

pub struct MirrorParams {
    pub reader: OwnedReadHalf,
    pub sender: DataSender<OwnedWriteHalf>,
    pub video: VideoThread,
    pub shared: Arc<Shared>,
    pub closed: watch::Receiver<bool>,
    pub units: mpsc::Receiver<Unit>,
    pub pool: mpsc::Sender<Vec<u8>>,
    pub conn: Arc<tokio::sync::Mutex<RtspConn>>,
    pub audio: Option<AudioLaunch>,
    pub size: (u32, u32),
}

impl MirrorPipeline {
    pub fn launch(params: MirrorParams) -> Self {
        let MirrorParams { reader, sender, mut video, shared, closed, units, pool, conn, audio, size } = params;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (first_tx, first_rx) = oneshot::channel();
        let mut tasks = vec![
            tokio::spawn(send_loop(sender, units, pool, Arc::clone(&shared), stop_rx.clone(), first_tx)),
            tokio::spawn(read_loop(reader, Arc::clone(&shared), stop_rx.clone())),
        ];
        let (feedback_tx, mut feedback_rx) = watch::channel(false);
        tasks.push(tokio::spawn(audio::feedback_loop(conn.clone(), stop_rx.clone(), feedback_tx)));
        {
            let shared = Arc::clone(&shared);
            let mut stop = stop_rx.clone();
            tasks.push(tokio::spawn(async move {
                tokio::select! {
                    _ = feedback_rx.wait_for(|c| *c) => shared.fail("el receptor ha dejado de responder a /feedback"),
                    _ = stop.changed() => {}
                }
            }));
        }
        if let Some(audio) = audio {
            tasks.push(tokio::spawn(audio_starter(first_rx, audio, conn, stop_rx)));
        }
        video.start();
        let encoder = video.encoder.clone();
        Self { shared, stop: stop_tx, video, tasks, closed, size, encoder }
    }

    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }

    pub fn failure(&self) -> Option<String> {
        self.shared.failure()
    }

    pub fn stats(&self) -> &MirrorStats {
        &self.shared.stats
    }

    pub async fn stop(mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.stop.send(true);
        self.video.shutdown();
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for MirrorPipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.stop.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn audio_starter(
    first: oneshot::Receiver<()>,
    audio: AudioLaunch,
    conn: Arc<tokio::sync::Mutex<RtspConn>>,
    mut stop: watch::Receiver<bool>,
) {
    tokio::select! {
        sent = first => if sent.is_err() { return },
        _ = stop.changed() => return,
    }
    tracing::info!("Primer fotograma enviado; se inicia el audio del sistema");
    let pipeline: Pipeline = audio.pending.launch(audio.params, conn);
    let _ = stop.changed().await;
    pipeline.stop().await;
}

async fn read_loop(mut reader: impl AsyncRead + Unpin, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => match read {
                Ok(0) | Err(_) => {
                    if !shared.stopping() {
                        shared.fail("el canal de datos de vídeo se ha cerrado");
                    }
                    return;
                }
                Ok(n) => tracing::debug!("{n} bytes recibidos en el canal de datos"),
            },
            _ = stop.changed() => return,
        }
    }
}

async fn send_loop<W: AsyncWrite + Unpin>(
    mut sender: DataSender<W>,
    mut units: mpsc::Receiver<Unit>,
    pool: mpsc::Sender<Vec<u8>>,
    shared: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
    first_frame: oneshot::Sender<()>,
) {
    let mut first_frame = Some(first_frame);
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.reset();
    let mut report = interval(STATS_INTERVAL);
    report.set_missed_tick_behavior(MissedTickBehavior::Delay);
    report.reset();
    let mut skipping = false;
    let mut last = (Instant::now(), 0u64, 0u64, 0u64);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            received = units.recv() => {
                let Some(mut unit) = received else { break };
                if !unit.keyframe && (skipping || unit.capture.elapsed() > MAX_QUEUED_AGE) {
                    skipping = true;
                    shared.want_keyframe.store(true, Ordering::Release);
                    MirrorStats::add(&shared.stats.dropped, 1);
                    let _ = pool.try_send(unit.buf);
                    continue;
                }
                skipping = false;
                match sender.send_unit(&mut unit).await {
                    Ok(sent) => {
                        MirrorStats::add(&shared.stats.sent, 1);
                        MirrorStats::add(&shared.stats.bytes, sent.bytes as u64);
                        if unit.keyframe {
                            MirrorStats::add(&shared.stats.keyframes, 1);
                        }
                        if let Some(first) = first_frame.take() {
                            let _ = first.send(());
                            heartbeat.reset();
                        }
                    }
                    Err(e) => {
                        if !shared.stopping() {
                            shared.fail(format!("no se pudo enviar vídeo a la TV: {e}"));
                        }
                        break;
                    }
                }
                let _ = pool.try_send(std::mem::take(&mut unit.buf));
            }
            _ = heartbeat.tick(), if first_frame.is_none() => {
                if let Err(e) = sender.send_heartbeat().await {
                    if !shared.stopping() {
                        shared.fail(format!("fallo en el latido del canal de vídeo: {e}"));
                    }
                    break;
                }
            }
            _ = report.tick() => log_stats(&shared.stats, &mut last),
        }
    }
}

fn log_stats(stats: &MirrorStats, last: &mut (Instant, u64, u64, u64)) {
    let now = Instant::now();
    let elapsed = now.duration_since(last.0).as_secs_f64().max(0.001);
    let (sent, bytes, encoded) = (
        MirrorStats::get(&stats.sent),
        MirrorStats::get(&stats.bytes),
        MirrorStats::get(&stats.encoded),
    );
    let encode_micros = MirrorStats::get(&stats.encode_micros);
    tracing::debug!(
        capturados = MirrorStats::get(&stats.captured),
        repetidos = MirrorStats::get(&stats.repeated),
        codificados = encoded,
        enviados = sent,
        descartados = MirrorStats::get(&stats.dropped),
        claves = MirrorStats::get(&stats.keyframes),
        kbps = ((bytes - last.2) as f64 * 8.0 / elapsed / 1000.0) as u64,
        fps_enviados = ((sent - last.1) as f64 / elapsed) as u64,
        codificacion_media_us = encode_micros.checked_div(encoded).unwrap_or(0),
        codificacion_max_us = MirrorStats::get(&stats.encode_max_micros),
        "estadísticas de vídeo"
    );
    *last = (now, sent, bytes, encoded);
}

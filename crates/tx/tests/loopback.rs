use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::Duration;

use rustplay_core::identity::PairingStore;
use rustplay_core::mdns::DiscoveredDevice;
use rustplay_core::{AIRPLAY_SERVICE, Error};
use rustplay_rx::{Receiver, ReceiverConfig, RxEvent};
use rustplay_tx::pairing::Method;
use rustplay_tx::{AudioOptions, Session};

async fn expect_event(
    handle: &mut rustplay_rx::ReceiverHandle,
    what: &str,
    matches: impl Fn(&RxEvent) -> bool,
) {
    let wait = async {
        while let Some(event) = handle.events().recv().await {
            if matches(&event) {
                return;
            }
        }
        panic!("el receptor cerró el canal de eventos esperando: {what}");
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("tiempo agotado esperando: {what}"));
}

// Uses the real audio device and registers mDNS records for a few seconds, so it is opt-in.
#[tokio::test]
#[ignore = "usa audio real y mDNS; ejecutar con --ignored"]
async fn streams_to_our_own_receiver_over_loopback() {
    let mut receiver = Receiver::start(ReceiverConfig {
        name: "rustplay-tx-loopback".into(),
        port: 0,
        audio: true,
        video: false,
        loopback_only: true,
        ..ReceiverConfig::default()
    })
    .await
    .expect("no se pudo iniciar el receptor");
    let port = match receiver.events().recv().await {
        Some(RxEvent::Listening { port }) => port,
        other => panic!("evento inesperado: {other:?}"),
    };
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let device = DiscoveredDevice {
        instance: "rustplay-tx-loopback".into(),
        service_type: AIRPLAY_SERVICE.into(),
        host: "localhost".into(),
        addrs: vec![peer.ip()],
        port,
        txt: HashMap::new(),
    };
    let store_path = std::env::temp_dir().join(format!("rustplay-tx-test-{port}.plist"));
    let mut store = PairingStore::load_from(store_path).unwrap();

    let mut session = Session::connect_to(
        peer,
        &device,
        &mut store,
        || async { Err::<String, _>(Error::protocol("no se esperaba pedir PIN")) },
        Some(vec![Method::FairPlayOnly]),
    )
    .await
    .expect("fallo al conectar");
    let stream = session
        .start_audio(AudioOptions::default())
        .await
        .expect("fallo al iniciar el audio");
    expect_event(&mut receiver, "AudioStarted", |e| matches!(e, RxEvent::AudioStarted { .. })).await;

    session.set_volume(-20.0).await.expect("fallo al ajustar el volumen");
    expect_event(&mut receiver, "Volume", |e| matches!(e, RxEvent::Volume { db } if (*db + 20.0).abs() < 0.01)).await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    let stats = stream.stats().expect("el flujo debe estar activo").clone();
    let sent = stats.sent.load(Ordering::Relaxed);
    assert!(sent > 250, "se esperaban unos 375 paquetes en 3 s, hubo {sent}");

    stream.stop().await.unwrap();
    expect_event(&mut receiver, "StreamStopped", |e| matches!(e, RxEvent::StreamStopped { .. })).await;
    session.close().await.unwrap();
    receiver.shutdown().await.unwrap();
}

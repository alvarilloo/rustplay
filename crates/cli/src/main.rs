use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::time::Duration;

use rustplay_core::Error;
use rustplay_core::identity::PairingStore;
use rustplay_core::mdns::DiscoveredDevice;
use rustplay_core::media::capture::list_monitors;
use rustplay_rx::{Receiver, ReceiverConfig, RxEvent};
use rustplay_tx::{AudioOptions, MirrorOptions, Session};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rustplay-cli",
    version,
    about = "Emisor y receptor AirPlay para Windows"
)]
struct Cli {
    #[arg(short, long, global = true, help = "Muestra información de depuración")]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Lista los dispositivos AirPlay de la red")]
    List {
        #[arg(long, default_value_t = 3, help = "Segundos de búsqueda")]
        timeout: u64,
    },
    #[command(about = "Recibe audio y duplicación de pantalla")]
    Recv {
        #[arg(long, help = "Nombre visible en los dispositivos Apple")]
        name: Option<String>,
        #[arg(long, default_value_t = 7000, help = "Puerto TCP")]
        port: u16,
        #[arg(long, help = "Desactiva la recepción de audio")]
        no_audio: bool,
        #[arg(long, help = "Desactiva la duplicación de pantalla")]
        no_video: bool,
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "",
            value_name = "CÓDIGO",
            help = "Pide un código de 4 cifras para conectarse (aleatorio si no se indica)"
        )]
        pin: Option<String>,
    },
    #[command(about = "Lista los monitores disponibles para duplicar")]
    Monitors,
    #[command(about = "Lista las ventanas que se pueden compartir")]
    Windows,
    #[command(about = "Envía audio o la pantalla a un dispositivo")]
    Send {
        #[arg(help = "Nombre o identificador del dispositivo")]
        device: String,
        #[arg(long, help = "Envía solo el audio del sistema")]
        audio_only: bool,
        #[arg(long, default_value_t = 0, help = "Índice del monitor")]
        monitor: u32,
        #[arg(long, help = "Comparte una sola ventana (identificador de `windows`) en lugar del monitor")]
        window: Option<isize>,
        #[arg(long, default_value_t = 30, help = "Fotogramas por segundo")]
        fps: u32,
        #[arg(long, default_value_t = 8_000_000, help = "Tasa de bits de vídeo")]
        bitrate: u32,
        #[arg(long, help = "Duplica solo la imagen, sin audio")]
        no_audio: bool,
        #[arg(
            long,
            allow_negative_numbers = true,
            help = "Volumen inicial en dB (de -144 a 0)"
        )]
        volume: Option<f32>,
        #[arg(long, default_value_t = 2000, help = "Latencia del audio en milisegundos")]
        latency: u32,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    let result = match cli.command {
        Command::List { timeout } => {
            let listed = list(timeout).await;
            if listed.is_ok() && cli.verbose {
                dump_txt(timeout).await
            } else {
                listed
            }
        }
        Command::Recv {
            name,
            port,
            no_audio,
            no_video,
            pin,
        } => recv(name, port, no_audio, no_video, pin).await,
        Command::Monitors => monitors(),
        Command::Windows => {
            windows();
            Ok(())
        }
        Command::Send {
            device,
            audio_only,
            monitor,
            window,
            fps,
            bitrate,
            no_audio,
            volume,
            latency,
        } => {
            if audio_only {
                send_audio(&device, volume, latency).await
            } else {
                let options = MirrorOptions { monitor, window, fps: fps.max(1), bitrate, audio: !no_audio, volume_db: volume, ..MirrorOptions::default() };
                send_mirror(&device, options).await
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", describe(&e));
            ExitCode::FAILURE
        }
    }
}

fn describe(error: &Error) -> String {
    match error {
        Error::Timeout => String::from(
            "tiempo de espera agotado. Comprueba que el dispositivo está encendido y en la misma red, y que el Firewall de Windows permite la aplicación en redes privadas (también UDP).",
        ),
        Error::Io(e) if e.kind() == io::ErrorKind::AddrInUse => String::from(
            "el puerto ya está en uso. Cierra la otra aplicación que lo usa o elige otro con --port.",
        ),
        Error::Io(e) if e.kind() == io::ErrorKind::PermissionDenied => format!(
            "acceso denegado ({e}). Comprueba que el Firewall de Windows permite la aplicación en redes privadas."
        ),
        Error::Windows(_) => format!(
            "{error}. Si el problema persiste, comprueba que el servicio «Cliente DNS» está activo y que el Firewall de Windows permite la aplicación en redes privadas."
        ),
        _ => error.to_string(),
    }
}

async fn list(timeout: u64) -> Result<(), Error> {
    let devices = rustplay_tx::discover(Duration::from_secs(timeout.max(1))).await?;
    if devices.is_empty() {
        println!("No se encontró ningún dispositivo AirPlay.");
        return Ok(());
    }
    let rows: Vec<[String; 4]> = devices
        .iter()
        .map(|d| {
            let address = d
                .addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(d.addrs.first())
                .map_or_else(
                    || String::from("-"),
                    |a| SocketAddr::new(*a, d.port).to_string(),
                );
            [
                d.display_name().to_owned(),
                d.txt
                    .get("model")
                    .cloned()
                    .unwrap_or_else(|| String::from("-")),
                address,
                d.device_id()
                    .map_or_else(|| id_from_instance(&d.instance), str::to_owned),
            ]
        })
        .collect();
    let headers = ["Nombre", "Modelo", "Dirección", "ID"];
    let mut widths = headers.map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let print_row = |cells: [&str; 4]| {
        let line: Vec<String> = cells
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    };
    print_row(headers);
    for row in &rows {
        print_row([&row[0], &row[1], &row[2], &row[3]]);
    }
    Ok(())
}

async fn dump_txt(timeout: u64) -> Result<(), Error> {
    for service in [rustplay_core::AIRPLAY_SERVICE, rustplay_core::RAOP_SERVICE] {
        let mut browser = rustplay_core::mdns::browse(service)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.max(1));
        while let Ok(Some(device)) = tokio::time::timeout_at(deadline, browser.next()).await {
            println!("\n{service} «{}» ({}:{})", device.instance, device.host, device.port);
            let mut txt: Vec<_> = device.txt.iter().collect();
            txt.sort();
            for (key, value) in txt {
                println!("  {key}={value}");
            }
        }
    }
    Ok(())
}

fn id_from_instance(instance: &str) -> String {
    match instance.split_once('@') {
        Some((mac, _)) if mac.len() == 12 && mac.is_ascii() => {
            let parts: Vec<&str> = (0..6).map(|i| &mac[i * 2..i * 2 + 2]).collect();
            parts.join(":")
        }
        _ => String::from("-"),
    }
}

async fn recv(
    name: Option<String>,
    port: u16,
    no_audio: bool,
    no_video: bool,
    pin: Option<String>,
) -> Result<(), Error> {
    let name = name
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty()))
        .unwrap_or_else(|| String::from("Windows"));
    let config = ReceiverConfig {
        name: name.clone(),
        port,
        audio: !no_audio,
        video: !no_video,
        texts: rustplay_rx::MirrorTexts {
            lock_title: String::from("Pantalla apagada"),
            lock_message: String::from("Enciende la pantalla de «{device}» para seguir duplicando."),
            paused_title: String::from("Conexión en pausa"),
            paused_message: String::from("Enciende la pantalla de «{device}» y vuelve a duplicar para continuar."),
            unknown_device: String::from("el iPhone"),
            ..Default::default()
        },
        pin: rustplay_rx::PinConfig {
            required: pin.is_some(),
            fixed: pin.filter(|p| !p.trim().is_empty()),
        },
        ..Default::default()
    };
    let mut handle = Receiver::start(config).await?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            _ = &mut ctrl_c => break,
            event = handle.events().recv() => match event {
                Some(event) => print_event(&name, &event),
                None => break,
            },
        }
    }
    println!("Deteniendo el receptor…");
    handle.shutdown().await
}

fn print_event(name: &str, event: &RxEvent) {
    match event {
        RxEvent::Listening { port } => {
            println!(
                "Receptor AirPlay «{name}» activo en el puerto {port}. Pulsa Ctrl+C para salir."
            );
        }
        RxEvent::Connected {
            peer,
            name: peer_name,
        } => match peer_name {
            Some(n) => println!("Conectado: {n} ({peer})"),
            None => println!("Conectado: {peer}"),
        },
        RxEvent::ClientName {
            peer,
            name: peer_name,
        } => println!("Dispositivo: {peer_name} ({peer})"),
        RxEvent::AudioStarted { peer, codec } => println!("Reproduciendo audio de {peer} ({codec:?})"),
        RxEvent::MirrorStarted { peer, width, height } => {
            println!("Duplicación de pantalla de {peer}: {width}x{height}")
        }
        RxEvent::MirrorStopped { peer } => println!("Duplicación de pantalla de {peer} detenida"),
        RxEvent::MirrorSuspended { peer } => println!("Duplicación de {peer} en pausa (pantalla bloqueada)"),
        RxEvent::MirrorResumed { peer } => println!("Duplicación de {peer} reanudada"),
        RxEvent::Volume { db } => println!("Volumen: {db:.1} dB"),
        RxEvent::StreamStopped { peer } => println!("Transmisión de {peer} detenida"),
        RxEvent::VideoStarted { peer } => println!("Vídeo recibido de {peer}"),
        RxEvent::VideoStopped => println!("Vídeo detenido"),
        RxEvent::PhotosStarted { peer } => println!("Fotos recibidas de {peer}"),
        RxEvent::PhotosStopped => println!("Fotos detenidas"),
        RxEvent::OpenApp => {}
        RxEvent::NowPlaying(now) => {
            let title = now.title.as_deref().unwrap_or("?");
            match now.artist.as_deref() {
                Some(artist) => println!("Sonando: {title} — {artist}"),
                None => println!("Sonando: {title}"),
            }
        }
        RxEvent::RecordingState { .. } => {}
        RxEvent::RecordingSaved { path } => println!("Grabación guardada en {}", path.display()),
        RxEvent::RecordingFailed { message } => println!("No se pudo grabar: {message}"),
        RxEvent::ScreenshotSaved { path } => println!("Captura guardada en {}", path.display()),
        RxEvent::ScreenshotFailed { message } => println!("No se pudo hacer la captura: {message}"),
        RxEvent::Disconnected { peer } => println!("Desconectado: {peer}"),
        RxEvent::DeviceSeen { device_id, name: peer_name, .. } => {
            println!("Identificado: {} ({device_id})", peer_name.as_deref().unwrap_or("sin nombre"))
        }
        RxEvent::AccessRequest { .. } | RxEvent::AccessResolved { .. } => {}
        RxEvent::AccessDenied { peer, device_id, .. } => {
            println!("Conexión rechazada: {device_id} ({peer})")
        }
        RxEvent::PinShown { peer, pin } => println!("*** Escribe el código {pin} en el dispositivo ({peer})"),
        RxEvent::PinFinished { peer, paired } => {
            if *paired {
                println!("Emparejado con código: {peer}")
            } else {
                println!("Código incorrecto o cancelado: {peer}")
            }
        }
    }
}

fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

fn hex_digits(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn device_hex_id(device: &DiscoveredDevice) -> String {
    let raw = device
        .device_id()
        .or_else(|| device.instance.split_once('@').map(|(mac, _)| mac))
        .unwrap_or_default();
    hex_digits(raw)
}

#[derive(Debug)]
enum Lookup<'a> {
    Found(&'a DiscoveredDevice),
    Ambiguous(Vec<&'a DiscoveredDevice>),
    Missing,
}

type Matcher<'a> = Box<dyn Fn(&DiscoveredDevice) -> bool + 'a>;

fn find_device<'a>(devices: &'a [DiscoveredDevice], query: &str) -> Lookup<'a> {
    let query = query.trim();
    let mut stages: Vec<Matcher<'_>> = Vec::new();
    if let Ok(ip) = query.parse::<IpAddr>() {
        stages.push(Box::new(move |d| d.addrs.contains(&ip)));
    }
    let id = hex_digits(query);
    let looks_like_id =
        id.len() == 12 && query.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '-');
    if looks_like_id {
        stages.push(Box::new(move |d| device_hex_id(d) == id));
    }
    let name = fold(query);
    stages.push(Box::new({
        let name = name.clone();
        move |d| fold(d.display_name()) == name
    }));
    stages.push(Box::new({
        let name = name.clone();
        move |d| !name.is_empty() && fold(d.display_name()).contains(&name)
    }));
    stages.push(Box::new(move |d| {
        d.txt.get("model").is_some_and(|m| fold(m) == name)
    }));
    for stage in stages {
        let mut hits: Vec<&DiscoveredDevice> = devices.iter().filter(|d| stage(d)).collect();
        match hits.len() {
            0 => {}
            1 => return Lookup::Found(hits.remove(0)),
            _ => return Lookup::Ambiguous(hits),
        }
    }
    Lookup::Missing
}

fn describe_device(device: &DiscoveredDevice) -> String {
    let address = device
        .addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(device.addrs.first())
        .map_or_else(|| String::from("sin dirección"), ToString::to_string);
    match device.txt.get("model") {
        Some(model) => format!("  - {} [{model}] ({address})", device.display_name()),
        None => format!("  - {} ({address})", device.display_name()),
    }
}

fn print_candidates(devices: &[&DiscoveredDevice]) {
    for device in devices {
        eprintln!("{}", describe_device(device));
    }
}

fn ask_pin() -> Result<String, Error> {
    print!("Introduce el PIN que aparece en la TV: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let pin = line.trim().to_owned();
    if pin.is_empty() {
        return Err(Error::Pairing(String::from("no se introdujo ningún PIN")));
    }
    Ok(pin)
}

async fn connect_session(query: &str) -> Result<(Session, String), Error> {
    let devices = rustplay_tx::discover(Duration::from_secs(3)).await?;
    let device = match find_device(&devices, query) {
        Lookup::Found(device) => device,
        Lookup::Ambiguous(matches) => {
            eprintln!(
                "Hay varios dispositivos que coinciden con «{query}». Sé más concreto o usa el identificador:"
            );
            print_candidates(&matches);
            return Err(Error::protocol("dispositivo ambiguo"));
        }
        Lookup::Missing => {
            if devices.is_empty() {
                eprintln!("No se encontró ningún dispositivo AirPlay en la red.");
            } else {
                eprintln!("No se encontró «{query}». Dispositivos disponibles:");
                print_candidates(&devices.iter().collect::<Vec<_>>());
            }
            return Err(Error::protocol("dispositivo no encontrado"));
        }
    };
    let name = device.display_name().to_owned();
    let mut store = PairingStore::load()?;
    let ask = || async {
        tokio::task::spawn_blocking(ask_pin)
            .await
            .map_err(|e| Error::protocol(e.to_string()))?
    };
    let session = Session::connect(device, &mut store, ask).await?;
    Ok((session, name))
}

fn windows() {
    let list = rustplay_core::media::capture::list_windows();
    if list.is_empty() {
        println!("No se encontró ninguna ventana que compartir.");
    }
    for w in list {
        println!("{}: {} ({})", w.hwnd, w.title, w.process);
    }
}

fn monitors() -> Result<(), Error> {
    let list = list_monitors()?;
    if list.is_empty() {
        println!("No se encontró ningún monitor.");
    }
    for m in list {
        println!("{}: {} ({}x{}, {})", m.index, m.name, m.width, m.height, m.adapter);
    }
    Ok(())
}

async fn send_mirror(query: &str, options: MirrorOptions) -> Result<(), Error> {
    let (mut session, name) = connect_session(query).await?;
    let stream = match session.start_mirror(options).await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = session.close().await;
            return Err(e);
        }
    };
    let (width, height) = stream.video_size().unwrap_or((0, 0));
    println!(
        "Duplicando la pantalla en «{name}» ({width}x{height}, {} fps). Pulsa Ctrl+C para detener.",
        options.fps
    );
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("Deteniendo la duplicación…"),
        () = stream.closed() => match stream.failure() {
            Some(reason) => eprintln!("Se detiene la duplicación: {reason}."),
            None => eprintln!("«{name}» ha dejado de responder; se detiene la duplicación."),
        },
    }
    let stopped = stream.stop().await;
    let closed = session.close().await;
    stopped.and(closed)
}

async fn send_audio(query: &str, volume: Option<f32>, latency_ms: u32) -> Result<(), Error> {
    let (mut session, name) = connect_session(query).await?;
    let stream = match session
        .start_audio(AudioOptions {
            latency_ms,
            volume_db: volume,
        }).await {
        Ok(stream) => stream,
        Err(e) => {
            let _ = session.close().await;
            return Err(e);
        }
    };
    println!("Enviando el audio del sistema a «{name}». Pulsa Ctrl+C para detener.");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("Deteniendo el envío…"),
        () = stream.closed() => eprintln!("«{name}» ha dejado de responder; se detiene el envío."),
    }
    let stopped = stream.stop().await;
    let closed = session.close().await;
    stopped.and(closed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn device(instance: &str, ip: &str, id: Option<&str>) -> DiscoveredDevice {
        let mut txt = HashMap::new();
        if let Some(id) = id {
            txt.insert(String::from("deviceid"), id.to_owned());
        }
        DiscoveredDevice {
            instance: instance.to_owned(),
            service_type: String::from("_airplay._tcp"),
            host: String::from("host.local"),
            addrs: vec![ip.parse().unwrap()],
            port: 7000,
            txt,
        }
    }

    fn devices() -> Vec<DiscoveredDevice> {
        vec![
            device("Salón", "192.168.1.20", Some("AA:BB:CC:DD:EE:01")),
            device("55\" QLED", "192.168.1.10", Some("B0:E4:5C:C3:5A:00")),
            device("B0E45CC35A01@Dormitorio TV", "192.168.1.30", None),
            device("Dormitorio Sonos", "192.168.1.31", Some("AA:BB:CC:DD:EE:02")),
        ]
    }

    fn found<'a>(lookup: &Lookup<'a>) -> &'a str {
        match lookup {
            Lookup::Found(d) => d.display_name(),
            other => panic!("se esperaba un dispositivo: {other:?}"),
        }
    }

    #[test]
    fn matches_name_ignoring_case_and_accents() {
        let list = devices();
        assert_eq!(found(&find_device(&list, "salon")), "Salón");
        assert_eq!(found(&find_device(&list, "SALÓN")), "Salón");
        assert_eq!(found(&find_device(&list, "55\" qled")), "55\" QLED");
    }

    #[test]
    fn matches_id_ip_and_unique_substring() {
        let list = devices();
        assert_eq!(found(&find_device(&list, "b0:e4:5c:c3:5a:00")), "55\" QLED");
        assert_eq!(found(&find_device(&list, "B0E45CC35A00")), "55\" QLED");
        assert_eq!(found(&find_device(&list, "192.168.1.30")), "Dormitorio TV");
        assert_eq!(found(&find_device(&list, "qled")), "55\" QLED");
        assert_eq!(found(&find_device(&list, "sonos")), "Dormitorio Sonos");
    }

    #[test]
    fn reports_ambiguous_and_missing() {
        let list = devices();
        match find_device(&list, "dormitorio") {
            Lookup::Ambiguous(hits) => assert_eq!(hits.len(), 2),
            other => panic!("se esperaba ambigüedad: {other:?}"),
        }
        assert!(matches!(find_device(&list, "cocina"), Lookup::Missing));
        assert!(matches!(find_device(&[], "salon"), Lookup::Missing));
    }

    #[test]
    fn exact_name_wins_over_substring() {
        let mut list = devices();
        list.push(device("Salón grande", "192.168.1.40", None));
        assert_eq!(found(&find_device(&list, "salón")), "Salón");
    }
}

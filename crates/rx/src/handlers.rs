use rustplay_core::Result;
use rustplay_core::identity::Identity;
use rustplay_core::rtsp::{Request, Response};
use bytes::Bytes;
use plist::{Dictionary, Value};
use tokio::sync::mpsc;

use crate::RxEvent;
use crate::txt::{self, Txt};

pub struct Context {
    pub name: String,
    pub identity: Identity,
    pub device_id: [u8; 6],
    pub public_key: [u8; 32],
    pub pairing_id: String,
    pub txt_raop: Txt,
    pub txt_airplay: Txt,
    pub events: mpsc::Sender<RxEvent>,
    pub live: std::sync::Arc<crate::Live>,
    pub prewarm: std::sync::Arc<crate::mirror::Prewarm>,
    pub remotes: std::sync::Arc<crate::dacp::Remotes>,
    pub sessions: std::sync::Arc<crate::devices::Sessions>,
    pub windows: std::sync::Arc<crate::devices::Windows<crate::mirror::Attach>>,
    pub access: std::sync::Arc<crate::access::AccessState>,
    pub video: std::sync::Arc<crate::video::Hub>,
    pub photo: std::sync::Arc<crate::photo::Hub>,
}

pub const BINARY_PLIST: &str = "application/x-apple-binary-plist";
const AUDIO_FORMATS: u64 = 0x3FF_FFFC;

pub fn uint(value: u64) -> Value {
    Value::Integer(value.into())
}

fn requested_qualifier(request: &Request) -> Option<String> {
    let content_type = request.headers.get("Content-Type")?;
    if !content_type.contains("application/x-apple-binary-plist") || request.body.is_empty() {
        return None;
    }
    let value = Value::from_reader(std::io::Cursor::new(&request.body[..])).ok()?;
    let qualifier = value
        .as_dictionary()?
        .get("qualifier")?
        .as_array()?
        .first()?
        .as_string()?;
    Some(qualifier.to_owned())
}

pub fn to_binary(dict: Dictionary) -> Result<Bytes> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(dict))?;
    Ok(Bytes::from(out))
}

pub fn info(ctx: &Context, request: &Request) -> Result<Response> {
    let mut root = Dictionary::new();

    if let Some(qualifier) = requested_qualifier(request) {
        match qualifier.as_str() {
            "txtAirPlay" => {
                root.insert(
                    "txtAirPlay".into(),
                    Value::Data(txt::wire_format(&ctx.txt_airplay)),
                );
            }
            "txtRAOP" => {
                root.insert(
                    "txtRAOP".into(),
                    Value::Data(txt::wire_format(&ctx.txt_raop)),
                );
            }
            _ => {}
        }
        return Ok(Response::ok(request.protocol).with_body(BINARY_PLIST, to_binary(root)?));
    }

    let device_id = txt::device_id_colon(&ctx.device_id);
    root.insert("deviceID".into(), Value::String(device_id.clone()));
    root.insert("macAddress".into(), Value::String(device_id));
    root.insert("pk".into(), Value::Data(ctx.public_key.to_vec()));
    root.insert(
        "features".into(),
        uint(u64::from(txt::FEATURES_HIGH) << 32 | u64::from(txt::features_low_with(ctx.video.enabled(), ctx.photo.enabled()))),
    );
    root.insert("name".into(), Value::String(ctx.name.clone()));
    root.insert("pi".into(), Value::String(ctx.pairing_id.clone()));
    root.insert("vv".into(), uint(2));
    root.insert("statusFlags".into(), uint(txt::STATUS_FLAGS));
    root.insert("keepAliveLowPower".into(), Value::Boolean(true));
    root.insert(
        "sourceVersion".into(),
        Value::String(txt::SOURCE_VERSION.into()),
    );
    root.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(true));
    root.insert("model".into(), Value::String(txt::MODEL.into()));

    if request.cseq().is_some() {
        root.insert("initialVolume".into(), Value::Real(-30.0));

        let latencies = [100u64, 101]
            .map(|kind| {
                let mut d = Dictionary::new();
                d.insert("type".into(), uint(kind));
                d.insert("inputLatencyMicros".into(), uint(0));
                d.insert("audioType".into(), Value::String("default".into()));
                d.insert("outputLatencyMicros".into(), Value::Boolean(false));
                Value::Dictionary(d)
            })
            .to_vec();
        root.insert("audioLatencies".into(), Value::Array(latencies));

        let formats = [100u64, 101]
            .map(|kind| {
                let mut d = Dictionary::new();
                d.insert("audioOutputFormats".into(), uint(AUDIO_FORMATS));
                d.insert("type".into(), uint(kind));
                d.insert("audioInputFormats".into(), uint(AUDIO_FORMATS));
                Value::Dictionary(d)
            })
            .to_vec();
        root.insert("audioFormats".into(), Value::Array(formats));

        let mut display = Dictionary::new();
        display.insert(
            "uuid".into(),
            Value::String("e0ff8a27-6738-3d56-8a16-cc53aacee925".into()),
        );
        display.insert("widthPhysical".into(), uint(0));
        display.insert("heightPhysical".into(), uint(0));
        let live = ctx.live.get();
        let (dw, dh) = (u64::from(live.display_width), u64::from(live.display_height));
        display.insert("width".into(), uint(dw));
        display.insert("height".into(), uint(dh));
        display.insert("widthPixels".into(), uint(dw));
        display.insert("heightPixels".into(), uint(dh));
        display.insert("rotation".into(), Value::Boolean(false));
        display.insert("refreshRate".into(), Value::Real(1.0 / f64::from(live.max_fps.max(1))));
        display.insert("maxFPS".into(), uint(u64::from(live.max_fps)));
        display.insert("overscanned".into(), Value::Boolean(false));
        display.insert("features".into(), uint(14));
        root.insert(
            "displays".into(),
            Value::Array(vec![Value::Dictionary(display)]),
        );
    }

    Ok(Response::ok(request.protocol).with_body(BINARY_PLIST, to_binary(root)?))
}

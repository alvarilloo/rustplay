use windows::Win32::Media::MediaFoundation::*;

use super::decoder::pcm_be_decode;
use super::*;
use crate::media::mf::{self, Mft};

const FRAMES: usize = 352;

fn fmt(codec: AudioCodec, frames: u32) -> AudioFormat {
    AudioFormat {
        codec,
        sample_rate: SAMPLE_RATE,
        channels: 2,
        frames_per_packet: frames,
    }
}

pub(crate) fn sine(frames: usize, offset: usize, amp: f32) -> Vec<i16> {
    let mut v = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let t = (offset + i) as f32 / SAMPLE_RATE as f32;
        let s = (amp * (2.0 * std::f32::consts::PI * 1000.0 * t).sin()) as i16;
        v.push(s);
        v.push(s);
    }
    v
}

fn rms(s: &[i16]) -> f64 {
    (s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
}

struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            if self.nbits % 8 == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.nbits % 8);
            self.nbits += 1;
        }
    }
}

fn alac_escape_frame(pcm: &[i16]) -> Vec<u8> {
    let mut w = BitWriter {
        bytes: Vec::new(),
        nbits: 0,
    };
    w.put(1, 3);
    w.put(0, 4);
    w.put(0, 12);
    w.put(0b1001, 4);
    w.put((pcm.len() / 2) as u32, 32);
    for &s in pcm {
        w.put(s as u16 as u32, 16);
    }
    w.put(7, 3);
    w.bytes
}

#[test]
fn pcm_byte_swap() {
    let packet = [0x01, 0x02, 0xff, 0xfe, 0x80, 0x00, 0x7f, 0xff];
    let mut out = [0i16; 4];
    assert_eq!(pcm_be_decode(&packet, &mut out).unwrap(), 2);
    assert_eq!(out, [0x0102, -2, i16::MIN, i16::MAX]);
    assert!(pcm_be_decode(&packet, &mut [0i16; 3]).is_err());
}

#[test]
fn pcm_decoder_via_factory() {
    let mut d = create_decoder(&fmt(AudioCodec::Pcm, 352)).unwrap();
    let mut out = [0i16; 2];
    assert_eq!(d.decode(&[0, 1, 1, 0], &mut out).unwrap(), 1);
    assert_eq!(out, [1, 256]);
}

#[test]
fn alac_cookie_layout() {
    let c = alac_cookie(352, 44_100, 2, 40, 10, 14);
    assert_eq!(c.len(), 24);
    assert_eq!(&c[0..4], &352u32.to_be_bytes());
    assert_eq!(&c[4..9], &[0, 16, 40, 10, 14]);
    assert_eq!(c[9], 2);
    assert_eq!(&c[10..12], &255u16.to_be_bytes());
    assert_eq!(&c[12..20], &[0; 8]);
    assert_eq!(&c[20..24], &44_100u32.to_be_bytes());
}

#[test]
fn aac_user_data_layout() {
    let d = aac_lc_user_data(44_100, 2);
    assert_eq!(&d[12..], &[0x12, 0x10]);
}

#[test]
fn eld_decoder_available() {
    assert!(create_decoder(&fmt(AudioCodec::AacEld, 480)).is_ok());
}

#[test]
fn alac_escape_frame_exact() {
    let mut d = create_decoder(&fmt(AudioCodec::Alac, FRAMES as u32)).unwrap();
    let mut out = vec![0i16; FRAMES * 2];
    for round in 0..3 {
        let pcm: Vec<i16> = (0..FRAMES * 2)
            .map(|i| ((i * 37 + round * 1001) as i32 % 65536 - 32768) as i16)
            .collect();
        let n = d.decode(&alac_escape_frame(&pcm), &mut out).unwrap();
        assert_eq!(n, FRAMES);
        assert_eq!(out, pcm);
    }
}

pub(crate) fn run_encoder(
    xf: windows::Win32::Media::MediaFoundation::IMFTransform,
    pcm: &[i16],
    chunk_frames: usize,
) -> Vec<Vec<u8>> {
    let mut enc = Mft::new(
        xf,
        None,
        16 * 1024,
        16 * 1024,
        chunk_frames as i64 * 10_000_000 / 44_100,
    )
    .unwrap();
    let mut packets = Vec::new();
    for c in pcm.chunks_exact(chunk_frames * 2) {
        let bytes: Vec<u8> = c.iter().flat_map(|s| s.to_le_bytes()).collect();
        enc.process(&bytes, &mut |p| {
            packets.push(p.to_vec());
            Ok(())
        })
        .unwrap();
    }
    enc.finish(&mut |p| {
        packets.push(p.to_vec());
        Ok(())
    })
    .unwrap();
    packets
}

#[test]
fn alac_roundtrip_ms_encoder() {
    let xf = mf::create_transform(&mf::CLSID_ALAC_ENCODER).unwrap();
    // SAFETY: negociación de tipos en una MFT de prueba.
    let cookie = alac_cookie(4096, SAMPLE_RATE, 2, 40, 10, 14).to_vec();
    // SAFETY: negociación de tipos en una MFT de prueba.
    unsafe {
        let t = MFCreateMediaType().unwrap();
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
        t.SetGUID(&MF_MT_SUBTYPE, &mf::MF_AUDIO_FORMAT_ALAC)
            .unwrap();
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).unwrap();
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
            .unwrap();
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).unwrap();
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4).unwrap();
        t.SetBlob(&MF_MT_USER_DATA, &cookie).unwrap();
        xf.SetOutputType(0, &t, 0).unwrap();
        xf.SetInputType(0, &mf::pcm_type(SAMPLE_RATE, 2).unwrap(), 0)
            .unwrap();
    }
    eprintln!(
        "cookie del codificador MS ({} bytes): {:02x?}",
        cookie.len(),
        cookie
    );
    let pcm = sine(FRAMES * 40, 0, 12000.0);
    let packets = run_encoder(xf, &pcm, FRAMES);
    assert!(!packets.is_empty());
    let mut d = create_alac_decoder(&fmt(AudioCodec::Alac, 4096), &cookie).unwrap();
    let mut out = vec![0i16; 4096 * 4];
    let mut decoded = Vec::new();
    for p in &packets {
        let n = d.decode(p, &mut out).unwrap();
        decoded.extend_from_slice(&out[..n * 2]);
    }
    eprintln!(
        "paquetes ALAC: {}, fotogramas: {}",
        packets.len(),
        decoded.len() / 2
    );
    assert!(decoded.len() / 2 >= FRAMES * 30);
    let n = decoded.len().min(pcm.len());
    assert_eq!(decoded[..n], pcm[..n]);
}

#[test]
fn aac_lc_roundtrip_ms_encoder() {
    let xf = mf::create_transform(&mf::CLSID_AAC_ENCODER).unwrap();
    // SAFETY: negociación de tipos en una MFT de prueba.
    unsafe {
        let t = MFCreateMediaType().unwrap();
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).unwrap();
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).unwrap();
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)
            .unwrap();
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).unwrap();
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20000)
            .unwrap();
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).unwrap();
        t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
            .unwrap();
        xf.SetOutputType(0, &t, 0).unwrap();
        xf.SetInputType(0, &mf::pcm_type(SAMPLE_RATE, 2).unwrap(), 0)
            .unwrap();
    }
    let pcm = sine(1024 * 60, 0, 12000.0);
    let packets = run_encoder(xf, &pcm, 1024);
    assert!(packets.len() >= 50, "paquetes: {}", packets.len());
    let mut d = create_decoder(&fmt(AudioCodec::AacLc, 1024)).unwrap();
    let mut out = vec![0i16; 4096];
    let mut decoded = Vec::new();
    let mut counts = Vec::new();
    for p in &packets {
        let n = d.decode(p, &mut out).unwrap();
        counts.push(n);
        decoded.extend_from_slice(&out[..n * 2]);
    }
    eprintln!("fotogramas por paquete: {:?}", &counts[..8]);
    assert!(counts.iter().skip(2).all(|&n| n == 1024));
    let tail = &decoded[decoded.len() / 2..];
    let ratio = rms(tail) / rms(&pcm);
    eprintln!("relación RMS: {ratio:.3}");
    assert!((0.85..1.15).contains(&ratio));
}

#[test]
#[ignore = "reproduce sonido en el dispositivo predeterminado"]
fn renderer_plays_tone() {
    let mut r = Renderer::start(100).unwrap();
    eprintln!(
        "periodo={} fotogramas, búfer={} fotogramas, latencia={:?}",
        r.period_frames(),
        r.buffer_frames(),
        r.device_latency()
    );
    r.set_volume(0.2);
    let tone: Vec<i16> = (0..SAMPLE_RATE as usize)
        .flat_map(|i| {
            let s =
                (8000.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin()) as i16;
            [s, s]
        })
        .collect();
    let mut sent = 0;
    let mut max_buffered = 0;
    while sent < SAMPLE_RATE as usize {
        sent += r.push(&tone[sent * 2..(sent * 2 + 882).min(tone.len())]);
        max_buffered = max_buffered.max(r.buffered_frames());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    eprintln!("máximo en cola: {max_buffered} fotogramas");
    std::thread::sleep(std::time::Duration::from_millis(300));
    r.flush();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(r.buffered_frames() < 2000);
}

#[test]
#[ignore = "requiere dispositivo de audio"]
fn loopback_starts_and_reads() {
    let mut l = LoopbackCapture::start().unwrap();
    let mut buf = vec![0i16; 4096];
    let n = l.read(&mut buf).unwrap();
    eprintln!("loopback leyó {n} fotogramas");
}

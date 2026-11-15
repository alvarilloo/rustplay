use windows::Win32::Media::MediaFoundation::*;

use super::{AudioCodec, AudioFormat, Decoder};
use crate::media::mf::{self, Mft};
use crate::{Error, Result};

const MAX_PACKET_BYTES: u32 = 16 * 1024;

pub fn alac_cookie(
    frames_per_packet: u32,
    sample_rate: u32,
    channels: u8,
    pb: u8,
    mb: u8,
    kb: u8,
) -> [u8; 24] {
    let mut c = [0u8; 24];
    c[0..4].copy_from_slice(&frames_per_packet.to_be_bytes());
    c[4] = 0;
    c[5] = 16;
    c[6] = pb;
    c[7] = mb;
    c[8] = kb;
    c[9] = channels;
    c[10..12].copy_from_slice(&255u16.to_be_bytes());
    c[12..16].copy_from_slice(&0u32.to_be_bytes());
    c[16..20].copy_from_slice(&0u32.to_be_bytes());
    c[20..24].copy_from_slice(&sample_rate.to_be_bytes());
    c
}

pub fn aac_lc_user_data(sample_rate: u32, channels: u16) -> [u8; 14] {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let idx = RATES.iter().position(|&r| r == sample_rate).unwrap_or(4) as u16;
    let asc: u16 = (2 << 11) | (idx << 7) | (channels << 3);
    let mut d = [0u8; 14];
    d[2..4].copy_from_slice(&0x29u16.to_le_bytes());
    d[12..14].copy_from_slice(&asc.to_be_bytes());
    d
}

pub(super) fn pcm_be_decode(packet: &[u8], out: &mut [i16]) -> Result<usize> {
    let samples = packet.len() / 2;
    if samples > out.len() {
        return Err(Error::Media("búfer de salida PCM demasiado pequeño".into()));
    }
    for (o, b) in out.iter_mut().zip(packet.chunks_exact(2)) {
        *o = i16::from_be_bytes([b[0], b[1]]);
    }
    Ok(samples / 2)
}

struct PcmDecoder;

impl Decoder for PcmDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        pcm_be_decode(packet, out)
    }
}

struct MftDecoder {
    mft: Mft,
}

impl Decoder for MftDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        let mut pos = 0usize;
        self.mft.process(packet, &mut |bytes| {
            let n = bytes.len() / 2;
            if pos + n > out.len() {
                return Err(Error::Media(
                    "búfer de salida de audio demasiado pequeño".into(),
                ));
            }
            for (o, b) in out[pos..pos + n].iter_mut().zip(bytes.chunks_exact(2)) {
                *o = i16::from_le_bytes([b[0], b[1]]);
            }
            pos += n;
            Ok(())
        })?;
        Ok(pos / 2)
    }
}

fn open_decoder(
    clsid: &windows::core::GUID,
    input: &IMFMediaType,
    format: &AudioFormat,
) -> Result<Box<dyn Decoder>> {
    let xf = mf::create_transform(clsid)?;
    let out = mf::pcm_type(format.sample_rate, format.channels as u32)?;
    // SAFETY: negociación de tipos sobre una MFT recién creada.
    unsafe {
        xf.SetInputType(0, input, 0)?;
        xf.SetOutputType(0, &out, 0)?;
    }
    let step =
        format.frames_per_packet.max(1) as i64 * 10_000_000 / format.sample_rate.max(1) as i64;
    let mft = Mft::new(xf, Some(out), MAX_PACKET_BYTES, 16 * 1024, step)?;
    Ok(Box::new(MftDecoder { mft }))
}

fn base_type(subtype: &windows::core::GUID, format: &AudioFormat) -> Result<IMFMediaType> {
    // SAFETY: llamadas COM sobre un tipo de medio recién creado.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, format.channels as u32)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, format.sample_rate)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        Ok(t)
    }
}

pub fn create_alac_decoder(format: &AudioFormat, cookie: &[u8]) -> Result<Box<dyn Decoder>> {
    mf::ensure_started()?;
    let t = base_type(&mf::MF_AUDIO_FORMAT_ALAC, format)?;
    // SAFETY: asignación de atributos sobre un tipo propio.
    unsafe {
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        t.SetBlob(&MF_MT_USER_DATA, cookie)?;
    }
    open_decoder(&mf::CLSID_ALAC_DECODER, &t, format)
}

pub(super) fn create(format: &AudioFormat) -> Result<Box<dyn Decoder>> {
    match format.codec {
        AudioCodec::Pcm => Ok(Box::new(PcmDecoder)),
        AudioCodec::Alac => {
            let cookie = alac_cookie(format.frames_per_packet, format.sample_rate, 2, 40, 10, 14);
            create_alac_decoder(format, &cookie)
        }
        AudioCodec::AacLc => {
            mf::ensure_started()?;
            let t = base_type(&MFAudioFormat_AAC, format)?;
            // SAFETY: asignación de atributos sobre un tipo propio.
            unsafe {
                t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
                t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1)?;
                t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16000)?;
                t.SetBlob(
                    &MF_MT_USER_DATA,
                    &aac_lc_user_data(format.sample_rate, format.channels),
                )?;
            }
            let xf = mf::create_transform(&mf::CLSID_AAC_DECODER)?;
            let out = mf::pcm_type(format.sample_rate, format.channels as u32)?;
            // SAFETY: negociación de tipos sobre una MFT recién creada.
            unsafe {
                xf.SetInputType(0, &t, 0)?;
                xf.SetOutputType(0, &out, 0)?;
            }
            let step = format.frames_per_packet.max(1) as i64 * 10_000_000
                / format.sample_rate.max(1) as i64;
            let mft = Mft::new(xf, Some(out), MAX_PACKET_BYTES, 16 * 1024, step)?;
            Ok(Box::new(MftDecoder { mft }))
        }
        AudioCodec::AacEld => Err(Error::NotImplemented("AAC-ELD")),
    }
}

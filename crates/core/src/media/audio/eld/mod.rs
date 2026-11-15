// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

mod bits;
mod channel;
mod config;
mod filterbank;
mod huffman;
mod stereo;
mod tables;
mod tns;

#[cfg(test)]
mod tests;

use super::{AudioFormat, Decoder};
use crate::{Error, Result};
use bits::BitReader;
use channel::{Channel, MAX_BANDS, decode_ics, decode_ics_info};
use config::{ASC_APPLE_ELD_44100, AudioSpecificConfig, FRAME_LENGTH, SwbInfo};
use filterbank::EldFilterbank;
use huffman::Codebooks;

const RANDOM_SEED: u32 = 0x1f2e3d4c;

pub struct EldDecoder {
    config: AudioSpecificConfig,
    swb: SwbInfo,
    codebooks: Codebooks,
    channels: [Channel; 2],
    filterbanks: [EldFilterbank; 2],
    ms_mask: [bool; MAX_BANDS],
    random_state: u32,
    pcm: [[f32; FRAME_LENGTH]; 2],
}

impl EldDecoder {
    pub fn new(asc: &[u8]) -> Result<Self> {
        let config = AudioSpecificConfig::parse(asc)?;
        let swb = config.swb_info()?;
        Ok(Self {
            config,
            swb,
            codebooks: Codebooks::new(),
            channels: [Channel::new(), Channel::new()],
            filterbanks: [EldFilterbank::new(), EldFilterbank::new()],
            ms_mask: [false; MAX_BANDS],
            random_state: RANDOM_SEED,
            pcm: [[0.0; FRAME_LENGTH]; 2],
        })
    }

    fn decode_elements(&mut self, br: &mut BitReader) -> Result<()> {
        let [ch0, ch1] = &mut self.channels;
        let cb = &self.codebooks;
        let swb = &self.swb;
        if self.config.channel_config == 1 {
            return decode_ics(br, cb, ch0, swb, &mut self.random_state, None);
        }
        let max_sfb = decode_ics_info(br, swb)?;
        let ms_present = br.read(2);
        if ms_present == 3 {
            return Err(Error::Media("ms_present reservado en AAC".into()));
        }
        match ms_present {
            1 => {
                for m in &mut self.ms_mask[..max_sfb] {
                    *m = br.read_bit() == 1;
                }
            }
            2 => self.ms_mask[..max_sfb].fill(true),
            _ => {}
        }
        decode_ics(br, cb, ch0, swb, &mut self.random_state, Some(max_sfb))?;
        decode_ics(br, cb, ch1, swb, &mut self.random_state, Some(max_sfb))?;
        if ms_present != 0 {
            stereo::apply_mid_side(ch0, ch1, &self.ms_mask, swb);
        }
        stereo::apply_intensity(ch0, ch1, ms_present != 0, &self.ms_mask, swb);
        Ok(())
    }

    fn synthesize(&mut self) {
        let count = if self.config.channel_config == 1 {
            1
        } else {
            2
        };
        for c in 0..count {
            let ch = &mut self.channels[c];
            if ch.tns.present {
                ch.tns.apply(&mut ch.coeffs, &self.swb, ch.max_sfb);
            }
            self.filterbanks[c].process(&mut ch.coeffs, &mut self.pcm[c]);
        }
        if count == 1 {
            self.pcm[1] = self.pcm[0];
        }
    }

    pub fn decode_frame(&mut self, packet: &[u8]) -> Result<()> {
        let mut br = BitReader::new(packet);
        self.decode_elements(&mut br)?;
        if br.overrun() {
            return Err(Error::Media("datos AAC-ELD truncados".into()));
        }
        self.synthesize();
        Ok(())
    }
}

fn to_i16(x: f32) -> i16 {
    x.round_ties_even().clamp(-32768.0, 32767.0) as i16
}

impl Decoder for EldDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        if out.len() < FRAME_LENGTH * 2 {
            return Err(Error::Media(
                "búfer de salida de audio demasiado pequeño".into(),
            ));
        }
        self.decode_frame(packet)?;
        for (i, frame) in out.chunks_exact_mut(2).take(FRAME_LENGTH).enumerate() {
            frame[0] = to_i16(self.pcm[0][i]);
            frame[1] = to_i16(self.pcm[1][i]);
        }
        Ok(FRAME_LENGTH)
    }
}

pub(super) fn create(_format: &AudioFormat) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(EldDecoder::new(&ASC_APPLE_ELD_44100)?))
}

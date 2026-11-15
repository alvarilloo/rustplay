// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

use super::bits::BitReader;
use super::tables::{SWB_OFFSET_480_24, SWB_OFFSET_480_32, SWB_OFFSET_480_48};
use crate::{Error, Result};

pub const AOT_ER_AAC_ELD: u32 = 39;
pub const FRAME_LENGTH: usize = 480;

pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

const NUM_SWB_480: [u8; 13] = [0, 0, 0, 35, 35, 37, 30, 30, 0, 0, 0, 0, 0];
const TNS_MAX_BANDS_480: [u8; 13] = [0, 0, 0, 31, 32, 37, 30, 30, 0, 0, 0, 0, 0];

pub const ASC_APPLE_ELD_44100: [u8; 4] = [0xF8, 0xE8, 0x50, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    pub object_type: u32,
    pub sampling_index: usize,
    pub sample_rate: u32,
    pub channel_config: u32,
    pub frame_length: usize,
    pub sbr: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SwbInfo {
    pub offsets: &'static [u16],
    pub num_swb: usize,
    pub tns_max_bands: usize,
}

fn media(msg: &str) -> Error {
    Error::Media(msg.into())
}

impl AudioSpecificConfig {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut br = BitReader::new(data);
        let mut object_type = br.read(5);
        if object_type == 31 {
            object_type = 32 + br.read(6);
        }
        let sampling_index = br.read(4) as usize;
        if sampling_index == 15 {
            return Err(media("frecuencia de muestreo AAC explícita no soportada"));
        }
        let sample_rate = *SAMPLE_RATES
            .get(sampling_index)
            .ok_or_else(|| media("índice de frecuencia AAC no válido"))?;
        let channel_config = br.read(4);
        if object_type != AOT_ER_AAC_ELD {
            return Err(media("el tipo de objeto AAC no es ER AAC ELD"));
        }
        let frame_length_short = br.read_bit() == 1;
        if br.read(3) != 0 {
            return Err(media("resiliencia a errores AAC-ELD no soportada"));
        }
        if br.read_bit() == 1 {
            return Err(media("AAC-ELD con SBR no soportado"));
        }
        loop {
            if br.read(4) == 0 {
                break;
            }
            let mut len = br.read(4) as usize;
            if len == 15 {
                len += br.read(8) as usize;
            }
            if len == 15 + 255 {
                len += br.read(16) as usize;
            }
            if br.bits_left() < len * 8 + 4 {
                return Err(media("configuración AAC-ELD truncada"));
            }
            br.skip(len * 8);
        }
        if br.read(2) != 0 {
            return Err(media("epConfig de AAC-ELD no soportado"));
        }
        if br.overrun() {
            return Err(media("configuración AAC-ELD truncada"));
        }
        let cfg = Self {
            object_type,
            sampling_index,
            sample_rate,
            channel_config,
            frame_length: if frame_length_short { 480 } else { 512 },
            sbr: false,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.frame_length != FRAME_LENGTH {
            return Err(media("AAC-ELD solo soporta tramas de 480 muestras"));
        }
        if !matches!(self.channel_config, 1 | 2) {
            return Err(media("configuración de canales AAC-ELD no soportada"));
        }
        self.swb_info().map(|_| ())
    }

    pub fn swb_info(&self) -> Result<SwbInfo> {
        let offsets: &'static [u16] = match self.sampling_index {
            3 | 4 => &SWB_OFFSET_480_48,
            5 => &SWB_OFFSET_480_32,
            6 | 7 => &SWB_OFFSET_480_24,
            _ => return Err(media("frecuencia no soportada por AAC-ELD")),
        };
        Ok(SwbInfo {
            offsets,
            num_swb: usize::from(NUM_SWB_480[self.sampling_index]),
            tns_max_bands: usize::from(TNS_MAX_BANDS_480[self.sampling_index]),
        })
    }
}

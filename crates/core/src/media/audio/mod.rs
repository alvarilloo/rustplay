use crate::Result;

mod alac_encoder;
mod decoder;
mod devices;
mod eld;
mod loopback;
mod renderer;
mod wasapi;

#[cfg(test)]
pub(crate) mod tests;

pub use alac_encoder::AlacEncoder;
pub use decoder::{aac_lc_user_data, alac_cookie, create_alac_decoder};
pub use devices::{AudioDevice, render_devices};
pub use loopback::{CaptureMode, LoopbackCapture};
pub use renderer::{MAX_EXTRA_DELAY_MS, Renderer};

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm,
    Alac,
    AacLc,
    AacEld,
}

pub trait Decoder: Send {
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames_per_packet: u32,
}

pub fn create_decoder(format: &AudioFormat) -> Result<Box<dyn Decoder>> {
    match format.codec {
        AudioCodec::AacEld => eld::create(format),
        _ => decoder::create(format),
    }
}

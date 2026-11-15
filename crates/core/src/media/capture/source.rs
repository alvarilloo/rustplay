use std::sync::Arc;
use std::time::Duration;

use super::{CapturedFrame, DesktopDuplicator, WindowCapture};
use crate::Result;
use crate::media::gpu::GpuDevice;

// What the sender shares: a whole monitor (DXGI output index) or a single top-level window (HWND).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    Monitor(u32),
    Window(isize),
}

pub enum Capturer {
    Desktop(Box<DesktopDuplicator>),
    Window(Box<WindowCapture>),
}

impl Capturer {
    pub fn new(gpu: Arc<GpuDevice>, source: CaptureSource, out_width: u32, out_height: u32) -> Result<Self> {
        Ok(match source {
            CaptureSource::Monitor(index) => {
                Self::Desktop(Box::new(DesktopDuplicator::new(gpu, index, out_width, out_height)?))
            }
            CaptureSource::Window(hwnd) => Self::Window(Box::new(WindowCapture::new(gpu, hwnd, out_width, out_height)?)),
        })
    }

    pub fn set_draw_cursor(&mut self, draw: bool) {
        match self {
            Self::Desktop(d) => d.set_draw_cursor(draw),
            Self::Window(w) => w.set_draw_cursor(draw),
        }
    }

    pub fn set_max_fps(&mut self, fps: u32) {
        match self {
            Self::Desktop(d) => d.set_max_fps(fps),
            Self::Window(w) => w.set_max_fps(fps),
        }
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>> {
        match self {
            Self::Desktop(d) => d.next_frame(timeout),
            Self::Window(w) => w.next_frame(timeout),
        }
    }

    pub fn last_frame(&self) -> Option<CapturedFrame<'_>> {
        match self {
            Self::Desktop(d) => d.last_frame(),
            Self::Window(w) => w.last_frame(),
        }
    }
}

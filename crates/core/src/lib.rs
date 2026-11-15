pub mod crypto;
pub mod error;
pub mod identity;
pub mod mdns;
pub mod net;
pub mod media;
pub mod ntp;
pub mod rtp;
pub mod rtsp;

pub use error::{Error, Result};

pub const AIRPLAY_SERVICE: &str = "_airplay._tcp";
pub const RAOP_SERVICE: &str = "_raop._tcp";

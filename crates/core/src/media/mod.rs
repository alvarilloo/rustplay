pub mod audio;
pub mod capture;
pub mod gpu;
pub mod photo;
pub mod player;
pub(crate) mod mf;
pub mod recorder;
pub mod reencode;
pub mod screenshot;
pub mod video;

// Prepares the calling thread for media work (COM in the multithreaded apartment, Media Foundation).
pub fn init_thread() -> crate::Result<()> {
    mf::ensure_started()
}

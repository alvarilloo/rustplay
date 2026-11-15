use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error de E/S: {0}")]
    Io(#[from] io::Error),
    #[error("error de Windows: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("plist no válido: {0}")]
    Plist(#[from] plist::Error),
    #[error("error de protocolo: {0}")]
    Protocol(String),
    #[error("error criptográfico: {0}")]
    Crypto(String),
    #[error("error de emparejamiento: {0}")]
    Pairing(String),
    #[error("error multimedia: {0}")]
    Media(String),
    #[error("tiempo de espera agotado")]
    Timeout,
    #[error("no implementado todavía: {0}")]
    NotImplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn protocol(msg: impl Into<String>) -> Self {
        Self::Protocol(msg.into())
    }
}

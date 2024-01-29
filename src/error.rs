use thiserror::Error;
use ipp::model::StatusCode;

#[derive(Error, Debug, Clone)]
#[error("{code} {msg:?}")]
pub struct IppError {
    pub code: StatusCode,
    pub msg: String,
}

#[derive(Error, Debug)]
pub struct Error {
    pub msg: String
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

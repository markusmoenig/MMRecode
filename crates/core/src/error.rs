//! Common error and result types.

use std::fmt;

/// An error reported by an `MMRecode` component.
#[derive(Debug)]
pub enum Error {
    /// The input is malformed or violates the relevant format.
    InvalidData(String),
    /// The requested format or feature is not implemented.
    Unsupported(String),
    /// A component was called in an invalid state.
    InvalidState(String),
    /// An underlying I/O operation failed.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(message) => write!(formatter, "invalid data: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::InvalidState(message) => write!(formatter, "invalid state: {message}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidData(_) | Self::Unsupported(_) | Self::InvalidState(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The common result type used by `MMRecode` crates.
pub type Result<T> = std::result::Result<T, Error>;

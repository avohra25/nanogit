//! Error type shared by every module.

use std::fmt;
use std::io;

use crate::zlib::ZlibError;

/// Everything that can go wrong inside nanogit.
#[derive(Debug)]
pub enum Error {
    /// An operating-system level I/O failure.
    Io(io::Error),
    /// A zlib/deflate stream could not be decoded.
    Zlib(ZlibError),
    /// On-disk data (object, index, ref) is malformed.
    Corrupt(String),
    /// A user-facing failure; the message is printed after `fatal: `.
    Fatal(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Build a [`Error::Fatal`] from anything string-like.
    pub fn fatal(msg: impl Into<String>) -> Error {
        Error::Fatal(msg.into())
    }

    /// Build a [`Error::Corrupt`] from anything string-like.
    pub fn corrupt(msg: impl Into<String>) -> Error {
        Error::Corrupt(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Zlib(e) => write!(f, "zlib error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt data: {m}"),
            Error::Fatal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<ZlibError> for Error {
    fn from(e: ZlibError) -> Error {
        Error::Zlib(e)
    }
}

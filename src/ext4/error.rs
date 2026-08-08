//! Error type used across the crate.

use std::fmt;

#[derive(Debug)]
pub enum ExtError {
    Io(std::io::Error),
    Corrupt(String),
    NotFound(String),
    NotDir(u32),
    Unsupported(String),
}

impl fmt::Display for ExtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtError::Io(e) => write!(f, "I/O error: {}", e),
            ExtError::Corrupt(m) => write!(f, "corrupt filesystem: {}", m),
            ExtError::NotFound(p) => write!(f, "no such file or directory: {}", p),
            ExtError::NotDir(i) => write!(f, "inode {} is not a directory", i),
            ExtError::Unsupported(m) => write!(f, "unsupported feature: {}", m),
        }
    }
}

impl std::error::Error for ExtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExtError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ExtError {
    fn from(e: std::io::Error) -> Self {
        ExtError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ExtError>;

//! Error types shared by the Rust implementation.
//!
//! Compatibility failures should normally be represented as explicit CLI exit
//! codes. This module is for lower-level failures that prevent normal command
//! reporting from completing.

use std::fmt;
use std::io;

use crate::dep_path::ResolveError;

/// Crate-wide result type used by the Rust implementation.
pub type Result<T> = std::result::Result<T, Error>;

/// Error type for operations that can fail before a command exit code exists.
///
/// Most user-facing CLI failures should still return the same exit codes and
/// messages as the Bash reference. This type is reserved for infrastructure
/// failures such as an output stream write failing before the CLI can finish
/// reporting a normal compatibility error.
#[derive(Debug)]
pub enum Error {
    /// A standard I/O operation failed.
    Io(io::Error),
    /// Dependency path resolution failed with a Bash-compatible class.
    Resolve(ResolveError),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Resolve(error) => write!(formatter, "{error:?}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

//! The kiewer server as a library, so the executable stays a thin wrapper.

pub mod blob;
pub mod error;

pub use error::{Error, Result};

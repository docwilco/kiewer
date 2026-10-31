//! The kiewer server as a library, so the executable stays a thin wrapper.

pub mod blob;
pub mod build;
pub mod config;
pub mod db;
pub mod error;
pub mod server;
pub mod web;

pub use error::{Error, Result};

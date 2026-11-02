//! The kiewer server as a library, so the executable stays a thin wrapper.

pub mod account;
pub mod auth;
pub mod blob;
pub mod build;
pub mod config;
pub mod db;
pub mod design;
pub mod error;
mod hex;
pub mod listing;
pub mod revision;
pub mod roles;
pub mod server;
pub mod session;
pub mod upload;
pub mod web;

pub use error::{Error, Result};

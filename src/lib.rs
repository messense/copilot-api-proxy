//! copilot-api-proxy - A reverse proxy for GitHub Copilot API

pub mod auth;
pub mod claude;
pub mod config;
pub mod error;
pub mod proxy;
pub mod server;

pub use error::Error;

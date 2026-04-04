//! copilot-api-proxy - A reverse proxy for GitHub Copilot API

pub mod amp;
pub mod amp_local;
pub mod auth;
pub mod claude;
pub mod config;
pub mod error;
pub mod gemini;
pub mod initiator;
pub mod proxy;
pub mod server;
pub mod token_counter;
pub mod web_backend;

pub use error::Error;

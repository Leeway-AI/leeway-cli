//! leeway — launch coding agents through the LeewayLLM gateway.
//!
//! Library form so unit and integration tests can exercise the relay, the
//! environment construction and the config handling in-process. The binary
//! (`src/main.rs`) is a thin clap dispatcher over these modules.

pub mod api;
pub mod cli;
pub mod config;
pub mod launch;
pub mod relay;
pub mod update;

pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

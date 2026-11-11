//! Library entry point for the AnimePahe GUI native backend.
//!
//! The regular desktop application uses `src/main.rs`. The Tauri shell links
//! against this library so both frontends share the same server, runner, and
//! download engine.

pub mod android;
pub mod api;
pub mod autocookie;
pub mod client;
pub mod config;
pub mod dns;
pub mod hls;
pub mod kwik;
pub mod library;
pub mod runner;
pub mod server;

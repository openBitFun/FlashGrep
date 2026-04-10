mod client;
mod managed;
pub mod protocol;
mod repo;
mod server;
mod service;

pub use client::DaemonClient;
pub use managed::{daemon_state_file_path, EnsuredRepo, ManagedDaemonClient};
pub use server::{serve, serve_stdio, ServerOptions};

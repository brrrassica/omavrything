//! Shared client library for `omavrything`.
//!
//! [`SocketClient`] speaks the newline-delimited JSON search protocol over the
//! Unix socket (the search hot path); [`DbusClient`] calls the control interface
//! on the system bus. Both are used by the CLI and the shell plugin.

pub mod dbus;
pub mod socket;

pub use dbus::DbusClient;
pub use socket::{SocketClient, default_socket_path};

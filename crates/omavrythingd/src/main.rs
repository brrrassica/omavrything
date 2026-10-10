//! The `omavrythingd` index daemon.
//!
//! Runs as a root system service, owns the in-memory index, and serves queries
//! over a Unix domain socket.

fn main() {
    println!("omavrythingd {} (scaffold)", omavrything_core::VERSION);
}

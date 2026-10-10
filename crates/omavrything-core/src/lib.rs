//! Core library for `omavrything`: the in-memory index, search engine,
//! filesystem scanner, and IPC protocol.

pub mod index;
pub mod protocol;
pub mod scan;

/// The crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}

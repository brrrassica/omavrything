//! NTFS MFT and USN journal access for `omavrything`.
//!
//! This crate isolates all FFI to libntfs-ng (ntfs-3g) behind a safe Rust
//! interface.

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

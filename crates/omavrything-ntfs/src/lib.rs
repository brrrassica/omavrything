//! NTFS MFT access for `omavrything`.
//!
//! This crate reads an NTFS volume's MFT directly from a raw device (or an
//! image) without mounting it, and detects NTFS partitions. It is a pure-Rust
//! implementation — the alternative documented in ADR-006 (see
//! `docs/decisions.md`) — so it has no build-time C dependency and can be tested
//! against a synthetic image.
//!
//! The reader is intentionally minimal: it parses the boot sector, reads the
//! `$MFT` record to obtain the MFT's run list, then walks every MFT record and
//! reconstructs paths from `$FILE_NAME` parent references. Directory index
//! structures are not needed, which is what makes raw-device indexing possible.

pub mod boot;
pub mod detect;
pub mod mft;
pub mod usn;
pub mod volume;

#[cfg(any(test, feature = "test-image"))]
pub mod image;

pub use boot::BootSector;
pub use detect::{NtfsPartition, is_ntfs_device, list_ntfs_partitions, probe_blkid};
pub use mft::{FileName, MftRecord};
pub use usn::{UsnRecord, parse_usn_records};
pub use volume::{NtfsEntry, NtfsVolume};

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

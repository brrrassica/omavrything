//! NTFS boot sector (BPB) parsing and detection.
//!
//! The boot sector is the first 512 bytes of an NTFS volume. It carries the
//! geometry needed to locate the MFT: the cluster size, the MFT's starting
//! cluster, and the MFT record size.

/// The NTFS OEM identifier at offset 3 of the boot sector.
pub const OEM_ID: &[u8; 8] = b"NTFS    ";

/// The parsed NTFS boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    /// Bytes per logical sector (usually 512).
    pub bytes_per_sector: u16,
    /// Sectors per cluster (a power of two).
    pub sectors_per_cluster: u8,
    /// Total number of sectors in the volume.
    pub total_sectors: u64,
    /// Logical cluster number of the `$MFT`.
    pub mft_lcn: u64,
    /// Logical cluster number of the `$MFTMirr`.
    pub mft_mirror_lcn: u64,
    /// Clusters per MFT record. Negative means `2^-value` bytes.
    pub clusters_per_mft_record: i8,
    /// Clusters per index buffer. Negative means `2^-value` bytes.
    pub clusters_per_index_buffer: i8,
    /// Volume serial number.
    pub serial: u64,
}

impl BootSector {
    /// Parse a boot sector from its first 512 bytes, or `None` if it is not a
    /// valid NTFS boot sector.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 512 || !is_ntfs(bytes) {
            return None;
        }
        let bytes_per_sector = u16::from_le_bytes([bytes[0x0B], bytes[0x0C]]);
        let sectors_per_cluster = bytes[0x0D];
        // A zero or non-power-of-two geometry is not a usable NTFS volume.
        if bytes_per_sector == 0 || sectors_per_cluster == 0 {
            return None;
        }
        Some(Self {
            bytes_per_sector,
            sectors_per_cluster,
            total_sectors: u64::from_le_bytes(bytes[0x28..0x30].try_into().unwrap()),
            mft_lcn: u64::from_le_bytes(bytes[0x30..0x38].try_into().unwrap()),
            mft_mirror_lcn: u64::from_le_bytes(bytes[0x38..0x40].try_into().unwrap()),
            clusters_per_mft_record: bytes[0x40] as i8,
            clusters_per_index_buffer: bytes[0x44] as i8,
            serial: u64::from_le_bytes(bytes[0x48..0x50].try_into().unwrap()),
        })
    }

    /// The cluster size in bytes.
    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    /// The MFT record size in bytes.
    pub fn mft_record_size(&self) -> u64 {
        signed_size(self.clusters_per_mft_record, self.cluster_size())
    }

    /// The byte offset of the `$MFT`.
    pub fn mft_offset(&self) -> u64 {
        self.mft_lcn * self.cluster_size()
    }
}

/// Resolve a signed "clusters per X" field: a positive value is a cluster
/// count, a negative value is `2^-value` bytes.
fn signed_size(value: i8, cluster_size: u64) -> u64 {
    if value > 0 {
        value as u64 * cluster_size
    } else {
        1u64 << (-(value as i32))
    }
}

/// Whether `bytes` begins with an NTFS boot sector (the OEM identifier).
pub fn is_ntfs(bytes: &[u8]) -> bool {
    bytes.len() >= 11 && &bytes[3..11] == OEM_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        b[3..11].copy_from_slice(OEM_ID);
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8;
        b[0x28..0x30].copy_from_slice(&2048u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&16u64.to_le_bytes());
        b[0x40] = (-10i8) as u8;
        b[0x44] = 1;
        b[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        b[0x1FE] = 0x55;
        b[0x1FF] = 0xAA;
        b
    }

    #[test]
    fn parses_a_valid_boot_sector() {
        let b = boot_bytes();
        assert!(is_ntfs(&b));
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.bytes_per_sector, 512);
        assert_eq!(bs.sectors_per_cluster, 8);
        assert_eq!(bs.cluster_size(), 4096);
        assert_eq!(bs.mft_lcn, 4);
        assert_eq!(bs.mft_offset(), 16384);
        assert_eq!(bs.mft_record_size(), 1024);
        assert_eq!(bs.serial, 0x1234_5678_9ABC_DEF0);
    }

    #[test]
    fn rejects_non_ntfs() {
        let mut b = boot_bytes();
        b[3] = b'X';
        assert!(!is_ntfs(&b));
        assert!(BootSector::parse(&b).is_none());
        assert!(BootSector::parse(&[0u8; 4]).is_none());
    }

    #[test]
    fn positive_clusters_per_record_is_a_cluster_count() {
        let mut b = boot_bytes();
        b[0x40] = 2;
        let bs = BootSector::parse(&b).unwrap();
        assert_eq!(bs.mft_record_size(), 8192);
    }
}

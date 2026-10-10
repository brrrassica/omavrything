//! USN change journal record parsing.
//!
//! The `$UsnJrnl:$J` stream is a sequence of `USN_RECORD_V2` entries. Parsing
//! them is implemented here; wiring live USN updates into the index is deferred
//! to v0.2 (ADR-006/ADR-010; see `docs/decisions.md`), so v0.1 uses MFT
//! read-only plus periodic rescan.

use crate::mft::ntfs_to_unix;

/// The file or directory was created.
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
/// The file or directory was deleted.
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
/// The old name of a rename.
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// The new name of a rename.
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// The file's data was overwritten.
pub const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;

/// A parsed `USN_RECORD_V2` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    /// The file's MFT reference (low 48 bits are the record number).
    pub file_ref: u64,
    /// The parent directory's MFT reference.
    pub parent_ref: u64,
    /// The update sequence number.
    pub usn: i64,
    /// The change time as Unix seconds.
    pub mtime: i64,
    /// The `USN_REASON_*` bitmask.
    pub reason: u32,
    /// The `FILE_ATTRIBUTE_*` bitmask.
    pub attributes: u32,
    /// The file name (UTF-8).
    pub name: String,
}

impl UsnRecord {
    /// The MFT record number of the file.
    pub fn file_number(&self) -> u64 {
        self.file_ref & 0x0000_FFFF_FFFF_FFFF
    }

    /// The MFT record number of the parent directory.
    pub fn parent_number(&self) -> u64 {
        self.parent_ref & 0x0000_FFFF_FFFF_FFFF
    }

    /// Whether the record is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & 0x1000_0000 != 0
    }
}

/// Parse a sequence of `USN_RECORD_V2` entries, stopping at the first malformed
/// or truncated record.
pub fn parse_usn_records(bytes: &[u8]) -> Vec<UsnRecord> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 0x3C <= bytes.len() {
        let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        if len < 0x3C || off + len > bytes.len() {
            break;
        }
        let major = u16::from_le_bytes([bytes[off + 4], bytes[off + 5]]);
        if major != 2 {
            off += len;
            continue;
        }
        let file_ref = u64::from_le_bytes(bytes[off + 0x08..off + 0x10].try_into().unwrap());
        let parent_ref = u64::from_le_bytes(bytes[off + 0x10..off + 0x18].try_into().unwrap());
        let usn = i64::from_le_bytes(bytes[off + 0x18..off + 0x20].try_into().unwrap());
        let timestamp = u64::from_le_bytes(bytes[off + 0x20..off + 0x28].try_into().unwrap());
        let reason = u32::from_le_bytes(bytes[off + 0x28..off + 0x2C].try_into().unwrap());
        let attributes = u32::from_le_bytes(bytes[off + 0x34..off + 0x38].try_into().unwrap());
        let name_len = u16::from_le_bytes([bytes[off + 0x38], bytes[off + 0x39]]) as usize;
        let name_off = u16::from_le_bytes([bytes[off + 0x3A], bytes[off + 0x3B]]) as usize;

        if name_len % 2 == 0 && off + name_off + name_len <= bytes.len() {
            let utf16: Vec<u16> = (0..name_len / 2)
                .map(|i| {
                    u16::from_le_bytes([
                        bytes[off + name_off + i * 2],
                        bytes[off + name_off + i * 2 + 1],
                    ])
                })
                .collect();
            if let Ok(name) = String::from_utf16(&utf16) {
                out.push(UsnRecord {
                    file_ref,
                    parent_ref,
                    usn,
                    mtime: ntfs_to_unix(timestamp),
                    reason,
                    attributes,
                    name,
                });
            }
        }
        off += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usn_record(name: &str, reason: u32) -> Vec<u8> {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = utf16.len() * 2;
        let len = (0x3C + name_bytes + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes()); // major version
        r[0x08..0x10].copy_from_slice(&(24u64 | (1u64 << 48)).to_le_bytes());
        r[0x10..0x18].copy_from_slice(&(5u64 | (1u64 << 48)).to_le_bytes());
        r[0x18..0x20].copy_from_slice(&42i64.to_le_bytes());
        let ticks = (1_700_000_000i64 + 11_644_473_600) as u64 * 10_000_000;
        r[0x20..0x28].copy_from_slice(&ticks.to_le_bytes());
        r[0x28..0x2C].copy_from_slice(&reason.to_le_bytes());
        r[0x38..0x3A].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        r[0x3A..0x3C].copy_from_slice(&0x3Cu16.to_le_bytes());
        for (i, u) in utf16.iter().enumerate() {
            r[0x3C + i * 2..0x3E + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_usn_records() {
        let mut bytes = usn_record("hello.txt", REASON_FILE_CREATE);
        bytes.extend_from_slice(&usn_record("subdir", REASON_RENAME_NEW_NAME));

        let records = parse_usn_records(&bytes);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "hello.txt");
        assert_eq!(records[0].file_number(), 24);
        assert_eq!(records[0].parent_number(), 5);
        assert_eq!(records[0].usn, 42);
        assert_eq!(records[0].mtime, 1_700_000_000);
        assert_eq!(records[0].reason, REASON_FILE_CREATE);
        assert!(!records[0].is_dir());
        assert_eq!(records[1].name, "subdir");
    }

    #[test]
    fn stops_at_a_truncated_record() {
        let mut bytes = usn_record("a.txt", REASON_FILE_CREATE);
        bytes.truncate(bytes.len() - 4);
        // The record length now exceeds the buffer, so nothing is parsed.
        assert!(parse_usn_records(&bytes).is_empty());
    }
}

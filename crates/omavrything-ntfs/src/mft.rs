//! MFT record, attribute, and data-run parsing.
//!
//! An NTFS volume's `$MFT` is an array of fixed-size records. Each record is a
//! `FILE` record carrying a set of attributes; the ones this crate needs are
//! `$FILE_NAME` (0x30) for the name, parent reference, and timestamps, and
//! `$DATA` (0x80) for the size and, for `$MFT` itself, the run list that locates
//! the rest of the MFT.

/// `$STANDARD_INFORMATION` attribute type.
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
/// `$FILE_NAME` attribute type.
pub const ATTR_FILE_NAME: u32 = 0x30;
/// `$DATA` attribute type.
pub const ATTR_DATA: u32 = 0x80;
/// The end-of-attributes marker.
pub const ATTR_END: u32 = 0xFFFF_FFFF;

/// The MFT record number of the root directory.
pub const ROOT_RECORD: u64 = 5;

/// The number of NTFS metadata records (0..=15) that are not user files.
pub const FIRST_USER_RECORD: u64 = 16;

/// A parsed `$FILE_NAME` attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// The parent directory's MFT record number.
    pub parent: u64,
    /// The file name (UTF-8).
    pub name: String,
    /// Last-modified time as Unix seconds.
    pub mtime: i64,
    /// The real (logical) size in bytes.
    pub real_size: u64,
    /// Whether the `FILE_ATTRIBUTE_DIRECTORY` flag is set.
    pub is_dir: bool,
    /// The name namespace: 0 POSIX, 1 Win32, 2 DOS, 3 Win32 & DOS.
    pub namespace: u8,
}

/// A parsed MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftRecord {
    /// The record number.
    pub number: u64,
    /// Whether the record is in use.
    pub in_use: bool,
    /// Whether the record is a directory.
    pub is_dir: bool,
    /// The `$FILE_NAME` attribute, if present.
    pub file_name: Option<FileName>,
    /// Last-modified time from `$STANDARD_INFORMATION` (Unix seconds), or 0.
    pub mtime: i64,
    /// The `$DATA` size in bytes (0 when absent).
    pub data_size: u64,
    /// The `$DATA` run list as `(start_lcn, cluster_count)` pairs.
    pub data_runs: Vec<(u64, u64)>,
}

/// Apply the update sequence array (fixup) to a raw MFT record in place.
///
/// NTFS overwrites the last two bytes of every sector with a sequence number;
/// the originals live in the update sequence array. Returns `false` if the
/// record is malformed or the sequence numbers do not match.
pub fn apply_fixups(record: &mut [u8], sector_size: usize) -> bool {
    if record.len() < 0x30 || sector_size == 0 {
        return false;
    }
    let usa_off = u16::from_le_bytes([record[0x04], record[0x05]]) as usize;
    let usa_count = u16::from_le_bytes([record[0x06], record[0x07]]) as usize;
    if usa_count < 2 || usa_off + usa_count * 2 > record.len() {
        return false;
    }
    let usn = u16::from_le_bytes([record[usa_off], record[usa_off + 1]]);
    for i in 1..usa_count {
        let end = i * sector_size;
        if end > record.len() {
            return false;
        }
        let found = u16::from_le_bytes([record[end - 2], record[end - 1]]);
        if found != usn {
            return false;
        }
        let orig = u16::from_le_bytes([record[usa_off + i * 2], record[usa_off + i * 2 + 1]]);
        record[end - 2..end].copy_from_slice(&orig.to_le_bytes());
    }
    true
}

/// Parse an MFT record. The record must already have had its fixups applied.
pub fn parse_record(bytes: &[u8], number: u64) -> Option<MftRecord> {
    if bytes.len() < 0x30 || &bytes[0..4] != b"FILE" {
        return None;
    }
    let flags = u16::from_le_bytes([bytes[0x16], bytes[0x17]]);
    let in_use = flags & 0x0001 != 0;
    let is_dir = flags & 0x0002 != 0;
    let attrs_off = u16::from_le_bytes([bytes[0x14], bytes[0x15]]) as usize;
    let used = u32::from_le_bytes([bytes[0x18], bytes[0x19], bytes[0x1A], bytes[0x1B]]) as usize;
    let end = used.min(bytes.len());

    let mut file_name: Option<FileName> = None;
    let mut mtime = 0i64;
    let mut data_size = 0u64;
    let mut data_runs = Vec::new();

    let mut off = attrs_off;
    while off + 8 <= end {
        let atype = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        if atype == ATTR_END {
            break;
        }
        let alen = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        if alen < 8 || off + alen > end {
            break;
        }
        let non_resident = bytes[off + 8] != 0;

        if atype == ATTR_STANDARD_INFORMATION && !non_resident {
            if let Some(value) = resident_value(bytes, off, alen) {
                if value.len() >= 0x10 {
                    let ticks = u64::from_le_bytes(value[0x08..0x10].try_into().unwrap());
                    mtime = ntfs_to_unix(ticks);
                }
            }
        } else if atype == ATTR_FILE_NAME && !non_resident {
            if let Some(value) = resident_value(bytes, off, alen) {
                if let Some(f) = parse_file_name(value) {
                    // Prefer a Win32/POSIX name over the DOS 8.3 alias, which
                    // may carry a zero size.
                    let replace = match &file_name {
                        None => true,
                        Some(existing) => existing.namespace == 2 && f.namespace != 2,
                    };
                    if replace {
                        file_name = Some(f);
                    }
                }
            }
        } else if atype == ATTR_DATA {
            // Only the unnamed `$DATA` stream is the file's contents; named
            // streams (e.g. `Zone.Identifier`) must not override its size.
            if bytes[off + 9] != 0 {
                off += alen;
                continue;
            }
            if non_resident {
                if off + 0x38 <= end {
                    data_size =
                        u64::from_le_bytes(bytes[off + 0x30..off + 0x38].try_into().unwrap());
                    let run_off =
                        u16::from_le_bytes([bytes[off + 0x20], bytes[off + 0x21]]) as usize;
                    if run_off >= 0x40 && off + run_off < off + alen {
                        data_runs = parse_data_runs(&bytes[off + run_off..off + alen]);
                    }
                }
            } else if let Some(value) = resident_value(bytes, off, alen) {
                data_size = value.len() as u64;
            }
        }

        off += alen;
    }

    Some(MftRecord {
        number,
        in_use,
        is_dir,
        file_name,
        mtime,
        data_size,
        data_runs,
    })
}

/// The resident attribute value at `off`, or `None` if the bounds are invalid.
fn resident_value(bytes: &[u8], off: usize, alen: usize) -> Option<&[u8]> {
    if off + 0x18 > bytes.len() {
        return None;
    }
    let vlen = u32::from_le_bytes(bytes[off + 0x10..off + 0x14].try_into().unwrap()) as usize;
    let voff = u16::from_le_bytes([bytes[off + 0x14], bytes[off + 0x15]]) as usize;
    if voff < 0x18 || voff + vlen > alen || off + voff + vlen > bytes.len() {
        return None;
    }
    Some(&bytes[off + voff..off + voff + vlen])
}

/// Parse a `$FILE_NAME` attribute value.
pub fn parse_file_name(value: &[u8]) -> Option<FileName> {
    if value.len() < 0x42 {
        return None;
    }
    let parent = u64::from_le_bytes(value[0..8].try_into().unwrap()) & 0x0000_FFFF_FFFF_FFFF;
    let mtime = u64::from_le_bytes(value[0x10..0x18].try_into().unwrap());
    let real_size = u64::from_le_bytes(value[0x30..0x38].try_into().unwrap());
    let flags = u32::from_le_bytes(value[0x38..0x3C].try_into().unwrap());
    let name_len = value[0x40] as usize;
    let name_end = 0x42 + name_len * 2;
    if name_end > value.len() {
        return None;
    }
    let utf16: Vec<u16> = (0..name_len)
        .map(|i| u16::from_le_bytes([value[0x42 + i * 2], value[0x43 + i * 2]]))
        .collect();
    let name = String::from_utf16(&utf16).ok()?;
    Some(FileName {
        parent,
        name,
        mtime: ntfs_to_unix(mtime),
        real_size,
        is_dir: flags & 0x1000_0000 != 0,
        namespace: value[0x41],
    })
}

/// Decode a non-resident attribute's data runs into `(start_lcn, cluster_count)`.
pub fn parse_data_runs(bytes: &[u8]) -> Vec<(u64, u64)> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    let mut prev_lcn: i64 = 0;
    while i < bytes.len() {
        let header = bytes[i];
        if header == 0 {
            break;
        }
        i += 1;
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || i + len_size + off_size > bytes.len() {
            break;
        }
        let mut len = 0u64;
        for j in 0..len_size {
            len |= (bytes[i + j] as u64) << (8 * j);
        }
        i += len_size;
        let mut off = 0i64;
        for j in 0..off_size {
            off |= (bytes[i + j] as i64) << (8 * j);
        }
        // Sign-extend the offset to 64 bits.
        if off_size > 0 && off_size < 8 {
            let shift = 64 - off_size * 8;
            off = (off << shift) >> shift;
        }
        i += off_size;
        prev_lcn += off;
        runs.push((prev_lcn as u64, len));
    }
    runs
}

/// Convert an NTFS timestamp (100 ns ticks since 1601-01-01) to Unix seconds.
pub fn ntfs_to_unix(ticks: u64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    (ticks / 10_000_000) as i64 - EPOCH_DIFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_data_runs() {
        // One run: length 8 clusters, offset +4.
        let runs = parse_data_runs(&[0x11, 0x08, 0x04, 0x00]);
        assert_eq!(runs, vec![(4, 8)]);

        // Two runs: (4, 8) then (20, 2) via a relative offset of +16.
        let runs = parse_data_runs(&[0x11, 0x08, 0x04, 0x11, 0x02, 0x10, 0x00]);
        assert_eq!(runs, vec![(4, 8), (20, 2)]);
    }

    #[test]
    fn decodes_negative_run_offsets() {
        // A run at LCN 100 then a sparse/backward run at LCN 90 (offset -10).
        let runs = parse_data_runs(&[0x11, 0x01, 0x64, 0x11, 0x01, 0xF6, 0x00]);
        assert_eq!(runs, vec![(100, 1), (90, 1)]);
    }

    #[test]
    fn converts_ntfs_timestamps() {
        // 2023-11-14T22:13:20Z = 1700000000 Unix seconds.
        let ticks = (1_700_000_000i64 + 11_644_473_600) as u64 * 10_000_000;
        assert_eq!(ntfs_to_unix(ticks), 1_700_000_000);
    }

    #[test]
    fn parses_a_file_name_value() {
        let mut v = vec![0u8; 0x42 + 9 * 2];
        v[0..8].copy_from_slice(&5u64.to_le_bytes());
        let ticks = (1_700_000_000i64 + 11_644_473_600) as u64 * 10_000_000;
        v[0x10..0x18].copy_from_slice(&ticks.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&1234u64.to_le_bytes());
        v[0x40] = 9;
        for (i, u) in "hello.txt".encode_utf16().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        let f = parse_file_name(&v).unwrap();
        assert_eq!(f.parent, 5);
        assert_eq!(f.name, "hello.txt");
        assert_eq!(f.real_size, 1234);
        assert_eq!(f.mtime, 1_700_000_000);
        assert!(!f.is_dir);
    }
}

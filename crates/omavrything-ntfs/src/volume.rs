//! Read an NTFS volume's MFT and enumerate its files.
//!
//! The reader is deliberately minimal: it parses the boot sector, reads the
//! `$MFT` record to obtain the MFT's run list, then walks every MFT record and
//! reconstructs paths from the `$FILE_NAME` parent references. It does not need
//! the directory index structures, which keeps it usable on a raw block device
//! without mounting.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};

use crate::boot::BootSector;
use crate::mft::{self, FIRST_USER_RECORD, MftRecord, ROOT_RECORD, VOLUME_RECORD};

/// One file or directory discovered in the MFT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsEntry {
    /// The path relative to the volume root, using `/` separators (no leading
    /// slash), e.g. `subdir/world.txt`.
    pub path: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The size in bytes (0 for directories).
    pub size: u64,
    /// Last-modified time as Unix seconds.
    pub mtime: i64,
}

/// A read-only NTFS volume backed by any `Read + Seek` source.
pub struct NtfsVolume<R: Read + Seek> {
    reader: R,
    boot: BootSector,
    mft_runs: Vec<(u64, u64)>,
    record_size: u64,
    record_count: u64,
}

impl<R: Read + Seek> NtfsVolume<R> {
    /// Open an NTFS volume, reading the boot sector and the `$MFT` record.
    pub fn open(mut reader: R) -> io::Result<Self> {
        let mut boot_bytes = [0u8; 512];
        reader.seek(SeekFrom::Start(0))?;
        reader.read_exact(&mut boot_bytes)?;
        let boot = BootSector::parse(&boot_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an NTFS volume"))?;

        let record_size = boot.mft_record_size();
        if record_size == 0 || record_size > 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "implausible MFT record size",
            ));
        }

        let mut record0 = vec![0u8; record_size as usize];
        reader.seek(SeekFrom::Start(boot.mft_offset()))?;
        reader.read_exact(&mut record0)?;
        if !mft::apply_fixups(&mut record0, boot.bytes_per_sector as usize) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad $MFT record fixups",
            ));
        }
        let record = mft::parse_record(&record0, 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad $MFT record"))?;
        if record.data_runs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "$MFT has no data runs",
            ));
        }

        let record_count = record.data_size / record_size;
        Ok(Self {
            reader,
            boot,
            mft_runs: record.data_runs,
            record_size,
            record_count,
        })
    }

    /// The volume's boot sector.
    pub fn boot(&self) -> &BootSector {
        &self.boot
    }

    /// The number of MFT records.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Read MFT record `number` from the volume's MFT runs, or `None` if the
    /// record lies beyond the MFT. The returned bytes have **not** had their
    /// fixups applied.
    fn read_mft_record(&mut self, number: u64) -> io::Result<Option<Vec<u8>>> {
        let record_size = self.record_size;
        let cluster = self.boot.cluster_size();
        let mut target = number * record_size;
        for &(lcn, len) in &self.mft_runs {
            let run_bytes = len * cluster;
            if target < run_bytes {
                let mut rec = vec![0u8; record_size as usize];
                self.reader.seek(SeekFrom::Start(lcn * cluster + target))?;
                self.reader.read_exact(&mut rec)?;
                return Ok(Some(rec));
            }
            target -= run_bytes;
        }
        Ok(None)
    }

    /// The `$VOLUME_INFORMATION` flags from the `$Volume` metadata file
    /// (record 3), or `None` if the record or attribute is unavailable.
    ///
    /// Bit [`crate::mft::VOLUME_DIRTY`] means Windows did not unmount the
    /// volume cleanly. This is a **read-only** probe used for diagnostics: it
    /// reads one MFT record and never writes to the device.
    pub fn volume_flags(&mut self) -> io::Result<Option<u16>> {
        let sector = self.boot.bytes_per_sector as usize;
        let Some(mut rec) = self.read_mft_record(VOLUME_RECORD)? else {
            return Ok(None);
        };
        if !mft::apply_fixups(&mut rec, sector) {
            return Ok(None);
        }
        Ok(mft::parse_volume_flags(&rec))
    }

    /// The maximum number of MFT bytes read into memory at once.
    const MFT_CHUNK_BYTES: usize = 16 * 1024 * 1024;

    /// Enumerate every user file and directory in the volume.
    ///
    /// Metadata records (0..=15) are skipped. Paths are relative to the volume
    /// root and use `/` separators.
    ///
    /// The MFT is read in bounded chunks rather than all at once, so a
    /// multi-gigabyte MFT does not have to be resident. A partial record at a
    /// chunk boundary is carried into the next chunk.
    pub fn enumerate(&mut self) -> io::Result<Vec<NtfsEntry>> {
        let sector = self.boot.bytes_per_sector as usize;
        let record_size = self.record_size as usize;
        let cluster = self.boot.cluster_size();

        // record number -> (parent, name, is_dir, size, mtime)
        let mut map: HashMap<u64, (u64, String, bool, u64, i64)> = HashMap::new();

        let chunk_bytes = (Self::MFT_CHUNK_BYTES / record_size).max(1) * record_size;
        let mut buf: Vec<u8> = Vec::new();
        let mut number: u64 = 0;
        for &(lcn, len) in &self.mft_runs {
            let run_bytes = len * cluster;
            let mut run_pos: u64 = 0;
            while run_pos < run_bytes {
                let want = (run_bytes - run_pos).min(chunk_bytes as u64) as usize;
                let carry = buf.len();
                buf.resize(carry + want, 0);
                self.reader.seek(SeekFrom::Start(lcn * cluster + run_pos))?;
                self.reader.read_exact(&mut buf[carry..])?;

                let mut pos = 0;
                while pos + record_size <= buf.len() {
                    let n = number;
                    number += 1;
                    let mut rec = buf[pos..pos + record_size].to_vec();
                    pos += record_size;
                    if !mft::apply_fixups(&mut rec, sector) {
                        continue;
                    }
                    let Some(parsed) = mft::parse_record(&rec, n) else {
                        continue;
                    };
                    if !parsed.in_use {
                        continue;
                    }
                    if let Some(f) = parsed.file_name {
                        // The `$DATA` real size is authoritative for files; the
                        // `$FILE_NAME` size is a fallback (and is 0 for the DOS
                        // alias).
                        let size = if parsed.is_dir {
                            0
                        } else if parsed.data_size > 0 {
                            parsed.data_size
                        } else {
                            f.real_size
                        };
                        // `$STANDARD_INFORMATION` is authoritative; `$FILE_NAME`
                        // is a secondary copy that can lag.
                        let mtime = if parsed.mtime != 0 {
                            parsed.mtime
                        } else {
                            f.mtime
                        };
                        map.insert(n, (f.parent, f.name, parsed.is_dir, size, mtime));
                    }
                }
                // Keep the partial record (if any) for the next chunk.
                buf.drain(..pos);
                run_pos += want as u64;
            }
        }

        let mut entries = Vec::new();
        for (&number, &(_, _, is_dir, size, mtime)) in &map {
            if number < FIRST_USER_RECORD {
                continue;
            }
            if let Some(path) = build_path(number, &map) {
                // Skip the NTFS metadata tree.
                if path == "$Extend" || path.starts_with("$Extend/") {
                    continue;
                }
                entries.push(NtfsEntry {
                    path,
                    is_dir,
                    size,
                    mtime,
                });
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
}

/// Reconstruct a path by walking `$FILE_NAME` parent references up to the root.
fn build_path(number: u64, map: &HashMap<u64, (u64, String, bool, u64, i64)>) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut current = number;
    let mut guard = 0;
    while current != ROOT_RECORD {
        let (parent, name, ..) = map.get(&current)?;
        parts.push(name);
        current = *parent;
        guard += 1;
        if guard > 256 {
            return None;
        }
    }
    parts.reverse();
    Some(parts.join("/"))
}

/// Parse an MFT record from raw bytes (fixups applied), for callers that read
/// records themselves.
pub fn parse_record(bytes: &[u8], number: u64) -> Option<MftRecord> {
    mft::parse_record(bytes, number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::build_test_image;
    use std::io::Cursor;

    #[test]
    fn enumerates_a_synthetic_volume() {
        let image = build_test_image();
        let mut volume = NtfsVolume::open(Cursor::new(image)).unwrap();
        assert_eq!(volume.record_count(), 32);

        let entries = volume.enumerate().unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["hello.txt", "subdir", "subdir/world.txt"]);

        let hello = entries.iter().find(|e| e.path == "hello.txt").unwrap();
        assert!(!hello.is_dir);
        assert_eq!(hello.size, 5);
        assert_eq!(hello.mtime, 1_700_000_000);

        let subdir = entries.iter().find(|e| e.path == "subdir").unwrap();
        assert!(subdir.is_dir);
        assert_eq!(subdir.size, 0);

        let world = entries
            .iter()
            .find(|e| e.path == "subdir/world.txt")
            .unwrap();
        assert_eq!(world.size, 3);
    }

    #[test]
    fn rejects_a_non_ntfs_image() {
        let image = vec![0u8; 4096];
        assert!(NtfsVolume::open(Cursor::new(image)).is_err());
    }
}

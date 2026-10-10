//! NTFS partition detection.
//!
//! Detection has two paths: a dependency-free boot-sector check (the OEM
//! identifier), and a `blkid` probe that also yields the UUID and label. The
//! `blkid` binary is used rather than binding `libblkid` so the crate has no
//! build-time C dependency.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::boot;

/// A detected NTFS partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsPartition {
    /// The block device path, e.g. `/dev/sda1`.
    pub device: PathBuf,
    /// The filesystem UUID, when known.
    pub uuid: Option<String>,
    /// The volume label, when known.
    pub label: Option<String>,
}

/// Whether `path` begins with an NTFS boot sector.
pub fn is_ntfs_device(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 512];
    file.read_exact(&mut buf)?;
    Ok(boot::is_ntfs(&buf))
}

/// Probe a single device with `blkid`, returning it when it is NTFS.
pub fn probe_blkid(device: &Path) -> Option<NtfsPartition> {
    let output = Command::new("blkid")
        .arg("-o")
        .arg("export")
        .arg(device)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut uuid = None;
    let mut label = None;
    let mut fstype = None;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "UUID" => uuid = Some(value.to_string()),
                "LABEL" => label = Some(value.to_string()),
                "TYPE" => fstype = Some(value.to_string()),
                _ => {}
            }
        }
    }
    if is_ntfs_type(fstype.as_deref()) {
        Some(NtfsPartition {
            device: device.to_path_buf(),
            uuid,
            label,
        })
    } else {
        None
    }
}

/// Enumerate all NTFS partitions on the system via `blkid`.
pub fn list_ntfs_partitions() -> Vec<NtfsPartition> {
    let output = match Command::new("blkid").arg("-o").arg("export").output() {
        Ok(output) if output.status.success() => output.stdout,
        _ => return Vec::new(),
    };
    parse_blkid_export(&String::from_utf8_lossy(&output))
}

/// Parse `blkid -o export` output (blank-line-separated device blocks).
pub fn parse_blkid_export(text: &str) -> Vec<NtfsPartition> {
    let mut out = Vec::new();
    let mut device: Option<String> = None;
    let mut uuid: Option<String> = None;
    let mut label: Option<String> = None;
    let mut fstype: Option<String> = None;

    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut out, &mut device, &mut uuid, &mut label, &mut fstype);
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "DEVNAME" => device = Some(value.to_string()),
                "UUID" => uuid = Some(value.to_string()),
                "LABEL" => label = Some(value.to_string()),
                "TYPE" => fstype = Some(value.to_string()),
                _ => {}
            }
        }
    }
    flush(&mut out, &mut device, &mut uuid, &mut label, &mut fstype);
    out
}

fn flush(
    out: &mut Vec<NtfsPartition>,
    device: &mut Option<String>,
    uuid: &mut Option<String>,
    label: &mut Option<String>,
    fstype: &mut Option<String>,
) {
    if let Some(dev) = device.take() {
        if is_ntfs_type(fstype.as_deref()) {
            out.push(NtfsPartition {
                device: PathBuf::from(dev),
                uuid: uuid.take(),
                label: label.take(),
            });
        }
    }
    *uuid = None;
    *label = None;
    *fstype = None;
}

fn is_ntfs_type(fstype: Option<&str>) -> bool {
    matches!(fstype, Some("ntfs") | Some("ntfs3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blkid_export_and_filters_ntfs() {
        let text = "\
DEVNAME=/dev/sda1
UUID=AAAA-BBBB
TYPE=ntfs
LABEL=Windows

DEVNAME=/dev/sda2
UUID=CCCC-DDDD
TYPE=ext4

DEVNAME=/dev/sdb1
UUID=EEEE-FFFF
TYPE=ntfs3
LABEL=Data
";
        let parts = parse_blkid_export(text);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].device, PathBuf::from("/dev/sda1"));
        assert_eq!(parts[0].uuid.as_deref(), Some("AAAA-BBBB"));
        assert_eq!(parts[0].label.as_deref(), Some("Windows"));
        assert_eq!(parts[1].device, PathBuf::from("/dev/sdb1"));
        assert_eq!(parts[1].label.as_deref(), Some("Data"));
    }

    #[test]
    fn empty_output_yields_no_partitions() {
        assert!(parse_blkid_export("").is_empty());
    }
}

//! Block-volume enumeration and indexing status.
//!
//! The settings UI lists every detected block filesystem (NTFS, ext4, btrfs,
//! …) with its current state. Detection uses `blkid -o export`, the same
//! dependency-free probe the NTFS path uses, and mount points come from
//! `/proc/mounts`. A volume is "indexed" when its mount point is one of the
//! index's roots; "indexing" is a transient state recorded while the daemon is
//! reading a volume (the NTFS MFT read or a targeted rescan).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use omavrything_core::mounts;

use crate::manager::IndexManager;

/// Devices currently being indexed. Keyed by the device path so a status query
/// can report "indexing" without holding the index lock.
static INDEXING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn indexing() -> &'static Mutex<HashSet<String>> {
    INDEXING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Marks a device as indexing for as long as the guard lives, clearing the
/// state on drop (including on an early return or panic).
pub struct IndexingGuard {
    device: String,
}

impl IndexingGuard {
    pub fn new(device: &Path) -> Self {
        let device = device.to_string_lossy().into_owned();
        indexing()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(device.clone());
        Self { device }
    }
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        indexing()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.device);
    }
}

/// Whether `device` is currently being indexed.
pub fn is_indexing(device: &Path) -> bool {
    let device = device.to_string_lossy();
    indexing()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(device.as_ref())
}

/// One detected block filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub device: PathBuf,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub fstype: String,
    pub mount_point: Option<PathBuf>,
}

impl Volume {
    /// Whether this is an NTFS volume (either kernel driver name).
    pub fn is_ntfs(&self) -> bool {
        matches!(self.fstype.as_str(), "ntfs" | "ntfs3")
    }
}

/// Enumerate every block filesystem `blkid` can identify.
///
/// Swap and empty signatures are skipped: they are not indexable volumes.
pub fn list_block_volumes() -> Vec<Volume> {
    let output = match Command::new("blkid").arg("-o").arg("export").output() {
        Ok(output) if output.status.success() => output.stdout,
        _ => return Vec::new(),
    };
    let mut volumes = parse_blkid_export(&String::from_utf8_lossy(&output));
    let mounts = mounts::read_mounts().unwrap_or_default();
    for volume in &mut volumes {
        volume.mount_point = mounts
            .iter()
            .find(|m| Path::new(&m.source) == volume.device)
            .map(|m| m.mount_point.clone());
    }
    // Sort by the parent device and then the partition number, so `/dev/sda1`
    // precedes `/dev/sda2` and NVMe partitions stay in numeric order.
    volumes.sort_by(|a, b| {
        let (a_parent, a_num) = partition_sort_key(&a.device);
        let (b_parent, b_num) = partition_sort_key(&b.device);
        a_parent
            .cmp(&b_parent)
            .then(a_num.cmp(&b_num))
            .then(a.device.cmp(&b.device))
    });
    volumes
}

/// Split a device path into its parent and trailing partition number.
///
/// `/dev/sda2` -> (`/dev/sda`, 2); `/dev/nvme0n1p3` -> (`/dev/nvme0n1`, 3);
/// a device without a trailing number -> (the whole path, 0).
fn partition_sort_key(device: &Path) -> (String, u32) {
    let text = device.to_string_lossy();
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    if end == bytes.len() {
        return (text.into_owned(), 0);
    }
    let number = text[end..].parse().unwrap_or(0);
    let mut parent_end = end;
    if parent_end > 0 && bytes[parent_end - 1] == b'p' {
        parent_end -= 1;
    }
    (text[..parent_end].to_string(), number)
}

/// Parse `blkid -o export` output (blank-line-separated device blocks).
fn parse_blkid_export(text: &str) -> Vec<Volume> {
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
    out: &mut Vec<Volume>,
    device: &mut Option<String>,
    uuid: &mut Option<String>,
    label: &mut Option<String>,
    fstype: &mut Option<String>,
) {
    if let Some(dev) = device.take() {
        if let Some(fs) = fstype.take() {
            if !fs.is_empty() && fs != "swap" {
                out.push(Volume {
                    device: PathBuf::from(dev),
                    uuid: uuid.take(),
                    label: label.take(),
                    fstype: fs,
                    mount_point: None,
                });
            }
        }
    }
    *uuid = None;
    *label = None;
    *fstype = None;
}

/// Probe a single device's filesystem type with `blkid`.
pub fn probe_fstype(device: &Path) -> Option<String> {
    let output = Command::new("blkid")
        .arg("-o")
        .arg("value")
        .arg("-s")
        .arg("TYPE")
        .arg(device)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let fstype = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!fstype.is_empty()).then_some(fstype)
}

/// The status shown in the settings UI: `unmounted`, `indexing`, or `indexed`.
pub fn status(volume: &Volume, index: &IndexManager) -> &'static str {
    if is_indexing(&volume.device) {
        return "indexing";
    }
    let Some(mount_point) = &volume.mount_point else {
        return "unmounted";
    };
    if index.roots().iter().any(|r| &r.path == mount_point) {
        "indexed"
    } else {
        "unmounted"
    }
}

/// Mount a non-NTFS volume through udisks2 and return its mount point.
///
/// A volume that is already mounted reuses its existing mount point. The mount
/// is writable (the desktop's normal behaviour); only the NTFS MFT path needs
/// the read-only invariant.
pub fn mount_generic(device: &Path) -> Result<PathBuf, String> {
    if let Some(mount_point) = existing_mount_point(device) {
        return Ok(mount_point);
    }
    let output = Command::new("udisksctl")
        .arg("mount")
        .arg("-b")
        .arg(device)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("could not mount {}: {stderr}", device.display()));
    }
    // udisksctl prints "Mounted /dev/sda1 at /run/media/user/Label."
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(pos) = stdout.find(" at ") {
        let mount_point = stdout[pos + 4..].trim().trim_end_matches('.').trim();
        if !mount_point.is_empty() {
            return Ok(PathBuf::from(mount_point));
        }
    }
    existing_mount_point(device).ok_or_else(|| {
        format!(
            "udisksctl mounted {} but its mount point could not be determined",
            device.display()
        )
    })
}

/// The mount point of `device` from `/proc/mounts`, if mounted.
pub fn existing_mount_point(device: &Path) -> Option<PathBuf> {
    // The daemon may run in a private mount namespace (systemd sandboxing), so
    // its own /proc/mounts hides the mounts it created in the host namespace.
    // Read the host table first.
    let text = std::fs::read_to_string("/proc/1/mounts")
        .or_else(|_| std::fs::read_to_string("/proc/mounts"))
        .ok()?;
    mounts::parse_mounts(&text)
        .into_iter()
        .find(|m| Path::new(&m.source) == device)
        .map(|m| m.mount_point)
}

/// Unmount `device` at `mount_point`.
///
/// Daemon-created private NTFS mounts are released with `umount` (retrying a
/// busy mount, then falling back to a lazy unmount). Everything else goes
/// through `udisksctl` so the desktop's view stays consistent, with `umount` as
/// a fallback.
pub fn unmount(device: &Path, mount_point: &Path) -> Result<(), String> {
    let private = mount_point.starts_with(crate::ntfs::PRIVATE_MOUNT_ROOT);
    if private {
        for _ in 0..3 {
            let ok = Command::new("umount")
                .arg(mount_point)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok && existing_mount_point(device).is_none() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = Command::new("umount").arg("-l").arg(mount_point).status();
    } else {
        let output = Command::new("udisksctl")
            .arg("unmount")
            .arg("-b")
            .arg(device)
            .output();
        if let Ok(output) = output {
            if output.status.success() && existing_mount_point(device).is_none() {
                return Ok(());
            }
        }
        let _ = Command::new("umount").arg(mount_point).status();
    }
    if existing_mount_point(device).is_some() {
        Err(format!("could not unmount {}", mount_point.display()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_filesystems_and_skips_swap() {
        let text = "\
DEVNAME=/dev/sda1
UUID=AAAA-BBBB
TYPE=ntfs
LABEL=Windows

DEVNAME=/dev/sda2
UUID=CCCC-DDDD
TYPE=ext4
LABEL=Root

DEVNAME=/dev/sda3
UUID=EEEE-FFFF
TYPE=swap
";
        let volumes = parse_blkid_export(text);
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].device, PathBuf::from("/dev/sda1"));
        assert_eq!(volumes[0].fstype, "ntfs");
        assert!(volumes[0].is_ntfs());
        assert_eq!(volumes[1].fstype, "ext4");
        assert!(!volumes[1].is_ntfs());
    }

    #[test]
    fn indexing_guard_clears_on_drop() {
        let device = Path::new("/dev/omavry-test-volume");
        assert!(!is_indexing(device));
        {
            let _guard = IndexingGuard::new(device);
            assert!(is_indexing(device));
        }
        assert!(!is_indexing(device));
    }
}

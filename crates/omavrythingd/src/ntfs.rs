//! NTFS partition mounting, auto-mount policy, and MFT-based indexing.
//!
//! Mounting prefers udisks2 (so the desktop sees the partition) and falls back
//! to a private read-only `ntfs3` mount. Indexing reads the MFT directly from
//! the raw device via `omavrything-ntfs`, so a partition can be indexed without
//! being mounted.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, RwLock};

use omavrything_core::index::{FsKind, Index};
use omavrything_ntfs::{NtfsEntry, NtfsPartition, NtfsVolume};

/// The private mount root used when udisks2 is unavailable.
const PRIVATE_MOUNT_ROOT: &str = "/run/omavrything/ntfs";

/// Mount an NTFS device read-only, preferring udisks2 and falling back to a
/// private `ntfs3` mount. Returns the mount point.
///
/// A device that is already mounted (e.g. by udisks2 at login) reuses its
/// existing mount point rather than attempting a second mount.
pub fn mount_readonly(device: &Path, uuid: Option<&str>) -> Result<PathBuf, String> {
    if let Some(mount_point) = existing_mount_point(device) {
        return Ok(mount_point);
    }
    if let Some(mount_point) = mount_via_udisks(device) {
        return Ok(mount_point);
    }
    mount_via_ntfs3(device, uuid)
}

/// The mount point of `device` if it is already mounted, from `/proc/mounts`.
pub fn existing_mount_point(device: &Path) -> Option<PathBuf> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let dev = device.to_string_lossy();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        let target = fields.next()?;
        if source == dev {
            return Some(PathBuf::from(unescape_mount(target)));
        }
    }
    None
}

/// Decode the octal escapes used by `/proc/mounts` (`\040` space, `\011` tab,
/// `\012` newline, `\134` backslash).
fn unescape_mount(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            if let Ok(n) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(n);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount via `udisksctl` (a udisks2 user mount), returning the mount point.
fn mount_via_udisks(device: &Path) -> Option<PathBuf> {
    let output = Command::new("udisksctl")
        .arg("mount")
        .arg("-b")
        .arg(device)
        .arg("-o")
        .arg("ro")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_udisks_mount_point(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the mount point from `udisksctl mount` output, e.g.
/// `Mounted /dev/sda1 at /run/media/sam/Data.`
fn parse_udisks_mount_point(text: &str) -> Option<PathBuf> {
    let idx = text.find(" at ")?;
    let rest = &text[idx + 4..];
    let end = rest.find('.').unwrap_or(rest.len());
    let path = rest[..end].trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Mount via the kernel `ntfs3` driver at a private read-only mount point.
fn mount_via_ntfs3(device: &Path, uuid: Option<&str>) -> Result<PathBuf, String> {
    let name = uuid.unwrap_or("volume");
    let mount_point = PathBuf::from(PRIVATE_MOUNT_ROOT).join(name);
    std::fs::create_dir_all(&mount_point).map_err(|e| e.to_string())?;
    let output = Command::new("mount")
        .arg("-t")
        .arg("ntfs3")
        .arg("-o")
        .arg("ro")
        .arg(device)
        .arg(&mount_point)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(mount_point)
}

/// Enumerate an NTFS device's MFT into a list of entries, without touching the
/// index.
///
/// This is the expensive half of NTFS indexing: it reads the whole MFT from the
/// raw device and parses every record, which takes seconds on a multi-million
/// record volume. Run it **outside** the index lock so searches keep serving,
/// then apply the result with [`apply_entries`] under a brief write lock.
pub fn enumerate_device(device: &Path) -> Result<Vec<NtfsEntry>, String> {
    let file = std::fs::File::open(device).map_err(|e| e.to_string())?;
    let mut volume = NtfsVolume::open(file).map_err(|e| e.to_string())?;
    volume.enumerate().map_err(|e| e.to_string())
}

/// Insert pre-enumerated NTFS entries into `index` under `mount_point`. Performs
/// no device I/O, so the caller can hold the index write lock for the duration.
/// Returns the number of entries added.
pub fn apply_entries(index: &mut Index, mount_point: &Path, entries: &[NtfsEntry]) -> usize {
    index.add_root(mount_point, FsKind::Ntfs);
    let mut added = 0usize;
    for entry in entries {
        let path = mount_point.join(&entry.path);
        if index.find(&path).is_none() {
            index.insert(&path, entry.is_dir, entry.size, entry.mtime);
            added += 1;
        }
    }
    added
}

/// Mount a partition, index it via the MFT, and remember it in the policy.
///
/// The mount happens before the index lock is taken, so a slow mount does not
/// block searches.
pub fn mount_index_and_remember(
    index: &RwLock<Index>,
    policy: &Mutex<NtfsPolicy>,
    partition: &NtfsPartition,
) -> Result<PathBuf, String> {
    let mount_point = mount_readonly(&partition.device, partition.uuid.as_deref())?;
    // Read and parse the MFT outside the index lock: on a multi-million-record
    // volume this takes seconds, and holding the write lock for it would stall
    // every search for the duration.
    let entries = enumerate_device(&partition.device)?;
    {
        let mut idx = index.write().unwrap_or_else(|e| e.into_inner());
        apply_entries(&mut idx, &mount_point, &entries);
    }
    if let Some(uuid) = &partition.uuid {
        let mut p = policy.lock().unwrap_or_else(|e| e.into_inner());
        let _ = p.remember(uuid);
    }
    Ok(mount_point)
}

/// The auto-mount policy: partitions that were previously indexed are
/// auto-mounted; newly seen ones are only offered.
#[derive(Debug, Default)]
pub struct NtfsPolicy {
    path: PathBuf,
    known: HashSet<String>,
}

impl NtfsPolicy {
    /// Load the policy from `path` (a newline-separated list of UUIDs).
    pub fn load(path: PathBuf) -> Self {
        let known = std::fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self { path, known }
    }

    /// Whether a partition with `uuid` was previously indexed.
    pub fn is_known(&self, uuid: &str) -> bool {
        self.known.contains(uuid)
    }

    /// The number of remembered partitions.
    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Whether the policy is empty.
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Remember a partition and persist the policy.
    pub fn remember(&mut self, uuid: &str) -> io::Result<()> {
        if self.known.insert(uuid.to_string()) {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text: Vec<&str> = self.known.iter().map(String::as_str).collect();
        text.sort_unstable();
        let mut body = text.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        std::fs::write(&self.path, body)
    }
}

/// The default policy path: `/var/lib` for the root system service, the XDG
/// state directory otherwise.
pub fn default_policy_path() -> PathBuf {
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/var/lib/omavrything/ntfs.state")
    } else {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("omavrything/ntfs.state")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(
            unescape_mount("/run/media/sam/My\\040Drive"),
            "/run/media/sam/My Drive"
        );
        assert_eq!(unescape_mount("/run/media/sam/Data"), "/run/media/sam/Data");
    }

    #[test]
    fn parses_udisks_mount_output() {
        let text = "Mounted /dev/sda1 at /run/media/sam/Data.\n";
        assert_eq!(
            parse_udisks_mount_point(text),
            Some(PathBuf::from("/run/media/sam/Data"))
        );
        assert_eq!(parse_udisks_mount_point("no mount here"), None);
    }

    #[test]
    fn indexes_a_synthetic_ntfs_image() {
        let dir = std::env::temp_dir().join(format!("omavrything-ntfs-idx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image_path = dir.join("volume.img");
        std::fs::write(&image_path, omavrything_ntfs::image::build_test_image()).unwrap();

        let mut index = Index::new();
        let mount_point = Path::new("/mnt/ntfs");
        let entries = enumerate_device(&image_path).unwrap();
        let added = apply_entries(&mut index, mount_point, &entries);
        assert_eq!(added, 3);

        assert!(index.find(Path::new("/mnt/ntfs/hello.txt")).is_some());
        assert!(index.find(Path::new("/mnt/ntfs/subdir")).is_some());
        assert!(
            index
                .find(Path::new("/mnt/ntfs/subdir/world.txt"))
                .is_some()
        );

        let hit = index.search("hello.txt", 10, 0);
        assert_eq!(hit.hits.len(), 1);
        assert_eq!(hit.hits[0].size, 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_and_apply_index_a_synthetic_image() {
        let dir =
            std::env::temp_dir().join(format!("omavrything-ntfs-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image_path = dir.join("volume.img");
        std::fs::write(&image_path, omavrything_ntfs::image::build_test_image()).unwrap();

        // The split path: enumerate outside the lock, then apply under it.
        let entries = enumerate_device(&image_path).unwrap();
        assert_eq!(entries.len(), 3);

        let mut index = Index::new();
        let mount_point = Path::new("/mnt/ntfs");
        let added = apply_entries(&mut index, mount_point, &entries);
        assert_eq!(added, 3);
        assert!(
            index
                .find(Path::new("/mnt/ntfs/subdir/world.txt"))
                .is_some()
        );

        // Applying the same entries again is idempotent.
        assert_eq!(apply_entries(&mut index, mount_point, &entries), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy_remembers_and_persists() {
        let dir = std::env::temp_dir().join(format!("omavrything-ntfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ntfs.state");

        let mut policy = NtfsPolicy::load(path.clone());
        assert!(policy.is_empty());
        assert!(!policy.is_known("AAAA-BBBB"));
        policy.remember("AAAA-BBBB").unwrap();
        assert!(policy.is_known("AAAA-BBBB"));

        // A fresh load sees the persisted UUID.
        let reloaded = NtfsPolicy::load(path);
        assert!(reloaded.is_known("AAAA-BBBB"));
        assert_eq!(reloaded.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Mount-table discovery.
//!
//! Parses `/proc/mounts` (the primary enumeration) and `/proc/self/mountinfo`
//! (for device identity), maps filesystem types to [`FsKind`], and identifies
//! pseudo-filesystems that should not be indexed.

use std::io;
use std::path::{Path, PathBuf};

use crate::index::FsKind;

/// One entry from `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub options: String,
}

/// One entry from `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub mount_id: u32,
    pub parent_id: u32,
    /// `major:minor` device identifier.
    pub device: String,
    pub root: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub source: String,
}

/// Read and parse `/proc/mounts`.
pub fn read_mounts() -> io::Result<Vec<Mount>> {
    Ok(parse_mounts(&std::fs::read_to_string("/proc/mounts")?))
}

/// Read and parse `/proc/self/mountinfo`.
pub fn read_mountinfo() -> io::Result<Vec<MountInfo>> {
    Ok(parse_mountinfo(&std::fs::read_to_string(
        "/proc/self/mountinfo",
    )?))
}

/// Parse `/proc/mounts` content.
pub fn parse_mounts(content: &str) -> Vec<Mount> {
    content.lines().filter_map(parse_mount_line).collect()
}

fn parse_mount_line(line: &str) -> Option<Mount> {
    let mut fields = line.split_whitespace();
    let source = fields.next()?;
    let mount_point = fields.next()?;
    let fs_type = fields.next()?;
    let options = fields.next().unwrap_or("");
    Some(Mount {
        source: unescape(source),
        mount_point: PathBuf::from(unescape(mount_point)),
        fs_type: fs_type.to_string(),
        options: options.to_string(),
    })
}

/// Parse `/proc/self/mountinfo` content.
pub fn parse_mountinfo(content: &str) -> Vec<MountInfo> {
    content.lines().filter_map(parse_mountinfo_line).collect()
}

fn parse_mountinfo_line(line: &str) -> Option<MountInfo> {
    // Fields before " - " are positional; after it are fstype, source, options.
    let sep = line.find(" - ")?;
    let (left, right) = line.split_at(sep);
    let right = &right[3..];

    let mut lf = left.split_whitespace();
    let mount_id = lf.next()?.parse().ok()?;
    let parent_id = lf.next()?.parse().ok()?;
    let device = lf.next()?.to_string();
    let root = lf.next()?.to_string();
    let mount_point = PathBuf::from(unescape(lf.next()?));

    let mut rf = right.split_whitespace();
    let fs_type = rf.next()?.to_string();
    let source = unescape(rf.next().unwrap_or(""));

    Some(MountInfo {
        mount_id,
        parent_id,
        device,
        root,
        mount_point,
        fs_type,
        source,
    })
}

/// Map a filesystem type to an [`FsKind`].
pub fn fs_kind(fs_type: &str) -> FsKind {
    match fs_type {
        "btrfs" => FsKind::Btrfs,
        "ext2" | "ext3" | "ext4" => FsKind::Ext4,
        "xfs" => FsKind::Xfs,
        "f2fs" => FsKind::F2fs,
        "vfat" | "fat" | "msdos" | "exfat" => FsKind::Vfat,
        "ntfs" | "ntfs3" | "fuseblk" => FsKind::Ntfs,
        _ => FsKind::Other,
    }
}

/// Whether a filesystem type is a kernel pseudo-filesystem that should not be
/// indexed.
pub fn is_pseudo_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "overlay"
            | "squashfs"
            | "ramfs"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "pstore"
            | "bpf"
            | "autofs"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "binfmt_misc"
            | "rpc_pipefs"
            | "nsfs"
            | "efivarfs"
    )
}

/// Whether a mount point is a pseudo path that should not be indexed.
pub fn is_pseudo_mount_point(path: &Path) -> bool {
    const PSEUDO: &[&str] = &["/proc", "/sys", "/dev", "/run", "/var/run", "/var/lock"];
    PSEUDO
        .iter()
        .any(|p| path == Path::new(p) || path.starts_with(p))
}

/// Decode the octal escapes used by `/proc/mounts` (`\040` space, `\011` tab,
/// `\012` newline, `\134` backslash).
fn unescape(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/mapper/omarchy_root / btrfs rw,relatime,ssd,space_cache=v2 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
tmpfs /run tmpfs rw,nosuid,nodev,relatime,size=3276800k 0 0
/dev/nvme0n1p1 /boot vfat rw,relatime,fmask=0022 0 0
/dev/sda1 /run/media/sam/My\\040Drive ntfs3 rw,relatime 0 0
";

    #[test]
    fn parses_mounts_and_unescapes() {
        let mounts = parse_mounts(MOUNTS);
        assert_eq!(mounts.len(), 6);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/"));
        assert_eq!(mounts[0].fs_type, "btrfs");
        assert_eq!(mounts[4].fs_type, "vfat");
        assert_eq!(
            mounts[5].mount_point,
            PathBuf::from("/run/media/sam/My Drive")
        );
    }

    #[test]
    fn maps_filesystem_kinds() {
        assert_eq!(fs_kind("btrfs"), FsKind::Btrfs);
        assert_eq!(fs_kind("ext4"), FsKind::Ext4);
        assert_eq!(fs_kind("xfs"), FsKind::Xfs);
        assert_eq!(fs_kind("f2fs"), FsKind::F2fs);
        assert_eq!(fs_kind("vfat"), FsKind::Vfat);
        assert_eq!(fs_kind("ntfs3"), FsKind::Ntfs);
        assert_eq!(fs_kind("zfs"), FsKind::Other);
    }

    #[test]
    fn identifies_pseudo_filesystems_and_paths() {
        assert!(is_pseudo_fs("proc"));
        assert!(is_pseudo_fs("tmpfs"));
        assert!(is_pseudo_fs("cgroup2"));
        assert!(!is_pseudo_fs("btrfs"));
        assert!(!is_pseudo_fs("ntfs3"));

        assert!(is_pseudo_mount_point(Path::new("/proc")));
        assert!(is_pseudo_mount_point(Path::new("/sys/fs/cgroup")));
        assert!(!is_pseudo_mount_point(Path::new("/process")));
        assert!(!is_pseudo_mount_point(Path::new("/home")));
    }

    #[test]
    fn parses_mountinfo() {
        let content = "\
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext4 /dev/root rw,errors=continue
25 30 0:23 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
";
        let infos = parse_mountinfo(content);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].mount_id, 36);
        assert_eq!(infos[0].parent_id, 35);
        assert_eq!(infos[0].device, "98:0");
        assert_eq!(infos[0].mount_point, PathBuf::from("/mnt2"));
        assert_eq!(infos[0].fs_type, "ext4");
        assert_eq!(infos[0].source, "/dev/root");
        assert_eq!(infos[1].fs_type, "proc");
    }
}

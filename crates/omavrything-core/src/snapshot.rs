//! Versioned, length-prefixed binary snapshot of the index.
//!
//! The format is:
//!
//! ```text
//! magic:      [u8; 8]  = b"OMAVSNAP"
//! version:    u32 LE
//! roots:      u32 count, then per root:
//!               fs_kind:    u8
//!               device:     u64
//!               generation: u64
//!               path:       u32 len + bytes
//! records:    u32 count, then per record:
//!               size, mtime, parent, path_off: u32
//!               path_len: u16, depth: u8, flags: u8
//! paths:      u32 len + bytes
//! checksum:   u64 LE (FNV-1a over everything before it)
//! ```
//!
//! Writes are atomic (temp file + `rename`) and the previous snapshot is kept as
//! `<path>.prev`, so a corrupt or truncated snapshot falls back to the last good
//! one and ultimately to a fresh scan.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::index::{FileRecord, FsKind, Index, Root};
use crate::scan::{ScanConfig, ScanReport, resolve_roots, scan, scan_into_index};

const MAGIC: &[u8; 8] = b"OMAVSNAP";
const VERSION: u32 = 3;

/// A snapshot error.
#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u32),
    Truncated,
    ChecksumMismatch { expected: u64, actual: u64 },
    InvalidUtf8,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "io error: {e}"),
            SnapshotError::BadMagic => write!(f, "not an omavrything snapshot"),
            SnapshotError::UnsupportedVersion(v) => write!(f, "unsupported snapshot version {v}"),
            SnapshotError::Truncated => write!(f, "snapshot is truncated"),
            SnapshotError::ChecksumMismatch { expected, actual } => write!(
                f,
                "snapshot checksum mismatch (expected {expected:#x}, got {actual:#x})"
            ),
            SnapshotError::InvalidUtf8 => write!(f, "snapshot contains invalid UTF-8"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

/// A per-root identity used to decide whether a snapshot's root is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootIdentity {
    /// `st_dev` of the root path.
    pub device: u64,
    /// btrfs subvolume generation, or 0 when unavailable.
    pub generation: u64,
}

/// The outcome of a warm start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmStart {
    /// The snapshot was valid; no scan was needed.
    Snapshot,
    /// The snapshot was loaded but some roots were rescanned.
    Partial { rescanned: usize },
    /// No usable snapshot; a full scan was performed.
    ColdScan,
}

/// The result of a warm start.
pub struct WarmStartResult {
    pub index: Index,
    pub report: ScanReport,
    pub identities: Vec<RootIdentity>,
    pub outcome: WarmStart,
}

/// The identity of the filesystem backing `path`, or `None` if it cannot be
/// read.
pub fn root_identity(path: &Path) -> Option<RootIdentity> {
    let meta = fs::metadata(path).ok()?;
    Some(RootIdentity {
        device: meta.dev(),
        generation: btrfs_generation(path).unwrap_or(0),
    })
}

/// The btrfs subvolume generation for `path`, or `None` if unavailable.
///
/// Uses `BTRFS_IOC_GET_SUBVOL_INFO`; the `generation` field sits at a fixed
/// offset in the kernel's `btrfs_ioctl_get_subvol_info_args`. A generously sized
/// buffer is used so a future kernel that grows the struct cannot overflow it.
fn btrfs_generation(path: &Path) -> Option<u64> {
    use std::os::unix::io::AsRawFd;

    // _IOR(0x94, 60, sizeof(struct btrfs_ioctl_get_subvol_info_args)) = 0x9210943c
    const BTRFS_IOC_GET_SUBVOL_INFO: libc::Ioctl = 0x9210_943c;
    // Offset of `generation` within the struct.
    const GENERATION_OFFSET: usize = 4064;
    // The kernel struct is 4624 bytes; over-allocate for forward compatibility.
    const BUFFER_LEN: usize = 8192;

    let file = File::open(path).ok()?;
    let mut buf = [0u8; BUFFER_LEN];
    // SAFETY: `buf` is larger than the kernel's struct, and the ioctl only
    // writes within it on success.
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            BTRFS_IOC_GET_SUBVOL_INFO,
            buf.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return None;
    }
    let mut generation_bytes = [0u8; 8];
    generation_bytes.copy_from_slice(&buf[GENERATION_OFFSET..GENERATION_OFFSET + 8]);
    Some(u64::from_ne_bytes(generation_bytes))
}

/// Serialize `index` and its root identities into a snapshot byte string.
pub fn encode(index: &Index, identities: &[RootIdentity]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);

    let roots = index.roots();
    put_u32(&mut out, roots.len() as u32);
    for (i, root) in roots.iter().enumerate() {
        let id = identities.get(i).copied().unwrap_or(RootIdentity {
            device: 0,
            generation: 0,
        });
        put_u8(&mut out, fs_kind_code(root.fs_kind));
        put_u64(&mut out, id.device);
        put_u64(&mut out, id.generation);
        put_bytes(&mut out, root.path.as_bytes());
    }

    let records = index.records();
    // Tombstones are not persisted, so the snapshot stays proportional to the
    // live set. Build a remap so `parent` links stay valid in the compacted
    // record list.
    let mut remap = vec![u32::MAX; records.len()];
    let mut live = 0u32;
    for (i, rec) in records.iter().enumerate() {
        if !rec.is_deleted() {
            remap[i] = live;
            live += 1;
        }
    }
    put_u32(&mut out, live);
    for rec in records.iter() {
        if rec.is_deleted() {
            continue;
        }
        let parent = if rec.parent == u32::MAX {
            u32::MAX
        } else {
            remap[rec.parent as usize]
        };
        put_u32(&mut out, rec.size);
        put_u32(&mut out, rec.mtime);
        put_u32(&mut out, parent);
        put_u32(&mut out, rec.path_off);
        put_u16(&mut out, rec.path_len);
        put_u8(&mut out, rec.depth);
        put_u8(&mut out, rec.flags);
    }

    put_bytes(&mut out, index.paths());

    let checksum = fnv1a64(&out);
    put_u64(&mut out, checksum);
    out
}

/// Deserialize a snapshot, returning the index and its root identities.
pub fn decode(bytes: &[u8]) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    if bytes.len() < MAGIC.len() + 4 + 8 {
        return Err(SnapshotError::Truncated);
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }

    let (body, tail) = bytes.split_at(bytes.len() - 8);
    let expected = u64::from_le_bytes(tail.try_into().unwrap());
    let actual = fnv1a64(body);
    if expected != actual {
        return Err(SnapshotError::ChecksumMismatch { expected, actual });
    }

    let mut r = Reader::new(body);
    r.skip(MAGIC.len())?;
    let version = r.u32()?;
    if version != VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }

    let root_count = r.u32()? as usize;
    let mut roots = Vec::with_capacity(root_count.min(4096));
    let mut identities = Vec::with_capacity(root_count.min(4096));
    for _ in 0..root_count {
        let fs_kind = fs_kind_from_code(r.u8()?);
        let device = r.u64()?;
        let generation = r.u64()?;
        let path = r.string()?;
        roots.push(Root { path, fs_kind });
        identities.push(RootIdentity { device, generation });
    }

    let record_count = r.u32()? as usize;
    // Each record is 4+4+4+4+2+1+1 = 20 bytes. Reject a count that cannot fit in
    // the remaining bytes before allocating, so a corrupt snapshot cannot force a
    // huge allocation; then pre-size exactly so the vector does not double.
    const RECORD_BYTES: usize = 20;
    if record_count.saturating_mul(RECORD_BYTES) > r.remaining() {
        return Err(SnapshotError::Truncated);
    }
    let mut records = Vec::with_capacity(record_count);
    for _ in 0..record_count {
        records.push(FileRecord {
            size: r.u32()?,
            mtime: r.u32()?,
            parent: r.u32()?,
            path_off: r.u32()?,
            path_len: r.u16()?,
            depth: r.u8()?,
            flags: r.u8()?,
        });
    }

    let paths = r.bytes()?.to_vec();

    // Validate arena offsets so a corrupt snapshot cannot cause out-of-bounds
    // reads later.
    for rec in &records {
        let path_end = rec.path_off as usize + rec.path_len as usize;
        if path_end > paths.len() {
            return Err(SnapshotError::Truncated);
        }
    }

    Ok((Index::from_parts(records, paths, roots), identities))
}

/// Write a snapshot atomically, keeping the previous one as `<path>.prev`.
pub fn save(index: &Index, identities: &[RootIdentity], path: &Path) -> io::Result<()> {
    let bytes = encode(index, identities);
    write_atomic(&bytes, path)
}

/// Write `bytes` to `path` atomically: a temp file, `fsync`, `rename`, and a
/// parent-directory `fsync` so the rename itself is durable. The previous
/// snapshot is kept as `<path>.prev`.
///
/// Callers that may run concurrently (the periodic writer and the shutdown
/// handler) must serialize their calls; the temp path is fixed.
pub fn write_atomic(bytes: &[u8], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = with_suffix(path, ".tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // Keep the previous snapshot as a fallback.
    if path.exists() {
        let _ = fs::rename(path, with_suffix(path, ".prev"));
    }
    fs::rename(&tmp, path)?;
    // Make the rename durable: fsync the containing directory.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Load a snapshot, falling back to `<path>.prev` if the primary is unusable.
pub fn load(path: &Path) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    match load_one(path) {
        Ok(loaded) => Ok(loaded),
        Err(primary) => match load_one(&with_suffix(path, ".prev")) {
            Ok(loaded) => Ok(loaded),
            Err(_) => Err(primary),
        },
    }
}

fn load_one(path: &Path) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

/// Build an index, warm-starting from `snapshot_path` when possible.
///
/// Roots whose identity or filesystem kind changed (or that are new) are
/// rescanned; roots that are no longer configured are dropped.
pub fn warm_start(config: &ScanConfig, snapshot_path: &Path) -> WarmStartResult {
    let roots = resolve_roots(config);

    let Ok((mut index, stored_ids)) = load(snapshot_path) else {
        let (index, report) = scan(config);
        let identities = roots
            .iter()
            .filter_map(|(path, _)| root_identity(path))
            .collect();
        return WarmStartResult {
            index,
            report,
            identities,
            outcome: WarmStart::ColdScan,
        };
    };

    // Map stored root path -> (fs_kind, identity).
    let stored: HashMap<String, (FsKind, RootIdentity)> = index
        .roots()
        .iter()
        .zip(stored_ids.iter())
        .map(|(root, id)| (root.path.clone(), (root.fs_kind, *id)))
        .collect();

    let mut report = ScanReport::default();
    // Identity per configured root path, keyed by path so the final vector can be
    // emitted in `index.roots()` order (which may differ from the config order).
    let mut id_by_path: HashMap<String, RootIdentity> = HashMap::new();
    let mut rescanned = 0usize;

    for (path, kind) in &roots {
        let path_str = path.to_string_lossy().into_owned();
        // A root that no longer exists is dropped rather than registered as an
        // empty phantom root.
        let Some(current) = root_identity(path) else {
            index.remove(path);
            continue;
        };
        let valid = stored
            .get(&path_str)
            .is_some_and(|(stored_kind, stored_id)| current == *stored_id && kind == stored_kind);
        if !valid {
            // Drop the stale subtree and re-walk this root.
            index.remove(path);
            let sub = ScanConfig {
                roots: vec![path.clone()],
                excludes: config.excludes.clone(),
                skip_pseudo: config.skip_pseudo,
                metadata: config.metadata,
            };
            let sub_report = scan_into_index(&mut index, &sub);
            report.entries += sub_report.entries;
            report.errors += sub_report.errors;
            rescanned += 1;
        }
        id_by_path.insert(path_str, current);
    }

    // Drop roots that are no longer configured (or no longer exist).
    let keep: HashSet<String> = id_by_path.keys().cloned().collect();
    let stale: Vec<PathBuf> = index
        .roots()
        .iter()
        .filter(|root| !keep.contains(&root.path))
        .map(|root| PathBuf::from(&root.path))
        .collect();
    for path in &stale {
        index.remove(path);
    }
    index.retain_roots(&keep);
    // Dropping stale roots tombstones their records but leaves their bytes in the
    // append-only arenas. Compact now so a subsequent re-index (e.g. NTFS roots
    // re-added at startup) appends to a small arena instead of doubling it.
    index.compact();

    // Emit identities aligned with `index.roots()` so a later `encode` zips them
    // correctly even if the config order changed between runs.
    let identities: Vec<RootIdentity> = index
        .roots()
        .iter()
        .map(|root| {
            id_by_path.get(&root.path).copied().unwrap_or(RootIdentity {
                device: 0,
                generation: 0,
            })
        })
        .collect();

    let outcome = if rescanned == 0 {
        WarmStart::Snapshot
    } else {
        WarmStart::Partial { rescanned }
    };
    WarmStartResult {
        index,
        report,
        identities,
        outcome,
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn fs_kind_code(kind: FsKind) -> u8 {
    match kind {
        FsKind::Btrfs => 0,
        FsKind::Ext4 => 1,
        FsKind::Xfs => 2,
        FsKind::F2fs => 3,
        FsKind::Vfat => 4,
        FsKind::Ntfs => 5,
        FsKind::Other => 6,
    }
}

fn fs_kind_from_code(code: u8) -> FsKind {
    match code {
        0 => FsKind::Btrfs,
        1 => FsKind::Ext4,
        2 => FsKind::Xfs,
        3 => FsKind::F2fs,
        4 => FsKind::Vfat,
        5 => FsKind::Ntfs,
        _ => FsKind::Other,
    }
}

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// FNV-1a 64-bit checksum.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A bounds-checked cursor over a snapshot body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The number of bytes left to read.
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self.pos.checked_add(n).ok_or(SnapshotError::Truncated)?;
        if end > self.buf.len() {
            return Err(SnapshotError::Truncated);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<(), SnapshotError> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> Result<&'a [u8], SnapshotError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String, SnapshotError> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| SnapshotError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("omavrything-snap-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_index() -> Index {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Btrfs);
        index.insert(Path::new("/home/sam/report.pdf"), false, 1234, 42);
        index.insert(Path::new("/home/sam/notes.md"), false, 10, 7);
        index.insert(Path::new("/home/sam"), true, 0, 0);
        index
    }

    #[test]
    fn round_trips_through_encode_decode() {
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 42,
            generation: 99,
        }];
        let bytes = encode(&index, &ids);
        let (back, back_ids) = decode(&bytes).unwrap();

        assert_eq!(back.len(), index.len());
        assert_eq!(back.roots().len(), 1);
        assert_eq!(back.roots()[0].fs_kind, FsKind::Btrfs);
        assert_eq!(back_ids, ids);

        let out = back.search("report", 10, 0);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].size, 1234);
        assert_eq!(out.hits[0].mtime, 42);
        assert_eq!(out.hits[0].path, "/home/sam/report.pdf");
    }

    #[test]
    fn decode_rejects_corruption() {
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 1,
            generation: 0,
        }];
        let good = encode(&index, &ids);

        // Bad magic.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(decode(&bad), Err(SnapshotError::BadMagic)));

        // Truncated header.
        assert!(matches!(decode(&good[..10]), Err(SnapshotError::Truncated)));

        // Flipped payload byte -> checksum mismatch.
        let mut flipped = good.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xff;
        assert!(matches!(
            decode(&flipped),
            Err(SnapshotError::ChecksumMismatch { .. })
        ));

        // Unsupported version (rewrite the version field, then fix the checksum
        // so the version check is reached).
        let mut versioned = good.clone();
        versioned[8..12].copy_from_slice(&999u32.to_le_bytes());
        let body_len = versioned.len() - 8;
        let checksum = fnv1a64(&versioned[..body_len]);
        versioned[body_len..].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            decode(&versioned),
            Err(SnapshotError::UnsupportedVersion(999))
        ));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("save-load");
        let path = dir.join("index.snapshot");
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 7,
            generation: 3,
        }];

        save(&index, &ids, &path).unwrap();
        let (back, back_ids) = load(&path).unwrap();
        assert_eq!(back.len(), index.len());
        assert_eq!(back_ids, ids);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_keeps_previous_as_fallback() {
        let dir = temp_dir("fallback");
        let path = dir.join("index.snapshot");
        let ids = vec![RootIdentity {
            device: 1,
            generation: 0,
        }];

        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/a.txt"), false, 1, 0);
        save(&index, &ids, &path).unwrap();

        // A second save moves the first snapshot to `.prev`.
        index.insert(Path::new("/b.txt"), false, 2, 0);
        save(&index, &ids, &path).unwrap();

        // Corrupt the primary; load must fall back to the previous snapshot.
        fs::write(&path, b"garbage").unwrap();
        let (loaded, _) = load(&path).unwrap();
        assert!(loaded.find(Path::new("/a.txt")).is_some());
        assert!(loaded.find(Path::new("/b.txt")).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_cold_then_snapshot() {
        let dir = temp_dir("warm");
        let data = dir.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("a.txt"), b"x").unwrap();
        let snap = dir.join("index.snapshot");
        let config = ScanConfig::with_roots(vec![data.clone()]);

        // No snapshot yet: cold scan.
        let cold = warm_start(&config, &snap);
        assert_eq!(cold.outcome, WarmStart::ColdScan);
        assert!(cold.index.find(&data.join("a.txt")).is_some());

        // Save, then warm-start: snapshot hit, no rescan.
        save(&cold.index, &cold.identities, &snap).unwrap();
        let warm = warm_start(&config, &snap);
        assert_eq!(warm.outcome, WarmStart::Snapshot);
        assert!(warm.index.find(&data.join("a.txt")).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_rescans_new_roots() {
        let dir = temp_dir("warm-partial");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("a.txt"), b"x").unwrap();
        fs::write(b.join("b.txt"), b"y").unwrap();
        let snap = dir.join("index.snapshot");

        let config_a = ScanConfig::with_roots(vec![a.clone()]);
        let cold = warm_start(&config_a, &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        // The config now includes a new root `b`, which must be scanned.
        let config_ab = ScanConfig::with_roots(vec![a.clone(), b.clone()]);
        let warm = warm_start(&config_ab, &snap);
        assert_eq!(warm.outcome, WarmStart::Partial { rescanned: 1 });
        assert!(warm.index.find(&a.join("a.txt")).is_some());
        assert!(warm.index.find(&b.join("b.txt")).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_drops_removed_roots() {
        let dir = temp_dir("warm-drop");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("a.txt"), b"x").unwrap();
        fs::write(b.join("b.txt"), b"y").unwrap();
        let snap = dir.join("index.snapshot");

        let config_ab = ScanConfig::with_roots(vec![a.clone(), b.clone()]);
        let cold = warm_start(&config_ab, &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        // `b` is no longer configured: its subtree must be dropped.
        let config_a = ScanConfig::with_roots(vec![a.clone()]);
        let warm = warm_start(&config_a, &snap);
        assert!(warm.index.find(&a.join("a.txt")).is_some());
        assert!(warm.index.find(&b.join("b.txt")).is_none());
        assert_eq!(warm.index.roots().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_omits_tombstones() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/keep.txt"), false, 1, 0);
        index.insert(Path::new("/gone.txt"), false, 2, 0);
        assert!(index.remove(Path::new("/gone.txt")));

        let ids = vec![RootIdentity {
            device: 1,
            generation: 0,
        }];
        let bytes = encode(&index, &ids);
        let (back, _) = decode(&bytes).unwrap();

        // Live records are the root and keep.txt; gone.txt is a tombstone.
        assert_eq!(back.len(), 2);
        assert!(back.find(Path::new("/keep.txt")).is_some());
        assert!(back.find(Path::new("/gone.txt")).is_none());
        // The snapshot holds only live records, not tombstones.
        assert_eq!(back.records().len(), 2);
    }

    #[test]
    fn warm_start_handles_reordered_roots() {
        let dir = temp_dir("warm-reorder");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("a.txt"), b"x").unwrap();
        fs::write(b.join("b.txt"), b"y").unwrap();
        let snap = dir.join("index.snapshot");

        let cold = warm_start(&ScanConfig::with_roots(vec![a.clone(), b.clone()]), &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        // The config order is reversed; both roots are unchanged, so no rescan.
        let warm = warm_start(&ScanConfig::with_roots(vec![b.clone(), a.clone()]), &snap);
        assert_eq!(warm.outcome, WarmStart::Snapshot);
        // Identities are aligned with the index's root order, not the config order.
        assert_eq!(warm.identities.len(), warm.index.roots().len());
        for (root, id) in warm.index.roots().iter().zip(warm.identities.iter()) {
            assert_eq!(Some(*id), root_identity(Path::new(&root.path)));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_drops_missing_root() {
        let dir = temp_dir("warm-missing");
        let a = dir.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("a.txt"), b"x").unwrap();
        let snap = dir.join("index.snapshot");

        let cold = warm_start(&ScanConfig::with_roots(vec![a.clone()]), &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        // `a` still exists; the second root does not and must not be registered.
        let missing = dir.join("does-not-exist");
        let warm = warm_start(
            &ScanConfig::with_roots(vec![a.clone(), missing.clone()]),
            &snap,
        );
        assert!(
            warm.index
                .roots()
                .iter()
                .all(|r| r.path != missing.to_string_lossy())
        );
        assert_eq!(warm.identities.len(), warm.index.roots().len());

        let _ = fs::remove_dir_all(&dir);
    }
}

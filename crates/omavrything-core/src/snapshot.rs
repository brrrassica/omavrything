//! Versioned, directly-mappable binary image of the index (v6).
//!
//! The format is:
//!
//! ```text
//! magic:         [u8; 8]  = b"OMAVIMG\0"
//! version:       u32 LE   (see [`VERSION`] / [`format_version`])
//! header_len:    u32 LE   (offset of the first section, 8-byte aligned)
//! section_count: u32 LE
//! section table: per section { kind: u32, _pad: u32, offset: u64, len: u64 }
//!   kind 1 = ROOTS        (root count u32, then per root:
//!                            fs_kind u8, _pad[7], device u64, generation u64,
//!                            mtime u64, path_off u64, path_len u32, _pad[4])
//!   kind 2 = RECORDS      (record count u32, _pad[4], then a contiguous array
//!                            of #[repr(C)] FileRecord)
//!   kind 3 = PATHS        (raw byte arena)
//!   kind 4 = ROOT_STRINGS (raw byte arena for root paths)
//! checksum:      u64 LE   (FNV-1a over everything before it)
//! ```
//!
//! Every section starts on an 8-byte boundary, so the `RECORDS` array is cast
//! directly from the mapping: loading is an `mmap` plus header validation, with
//! no decode and no rebuild. The `PATHS` and `ROOT_STRINGS` arenas are raw
//! bytes; records reference them by `path_off`/`path_len`.
//!
//! The format is explicitly versioned: [`IndexImage::from_bytes`] rejects any
//! version other than [`VERSION`] rather than misreading a future or past
//! layout. Writes are atomic (temp file + `rename`) and the previous snapshot is
//! kept as `<path>.prev`, so a corrupt or truncated snapshot falls back to the
//! last good one and ultimately to a fresh scan.
//!
//! A snapshot can be loaded either by reading the whole file into memory
//! ([`load`]) or by memory-mapping it ([`load_mmap`], F6); the latter avoids the
//! intermediate read buffer. [`warm_start_with`] selects between them.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::index::{FileRecord, FsKind, Index, Root, SearchOutcome};
use crate::scan::{ScanConfig, ScanReport, resolve_roots, scan, scan_into_index};

const MAGIC: &[u8; 8] = b"OMAVIMG\0";
/// The current snapshot format version. Bump it whenever the on-disk layout
/// changes; [`IndexImage::from_bytes`] rejects any other version rather than
/// misreading it.
pub const VERSION: u32 = 6;

/// Section kind: the root table (fs kind, identity, and path range).
const SECTION_ROOTS: u32 = 1;
/// Section kind: the contiguous `#[repr(C)]` record array.
const SECTION_RECORDS: u32 = 2;
/// Section kind: the raw path arena.
const SECTION_PATHS: u32 = 3;
/// Section kind: the raw root-path string arena.
const SECTION_ROOT_STRINGS: u32 = 4;

/// The fixed part of the header: magic(8) + version(4) + header_len(4) +
/// section_count(4).
const HEADER_FIXED: usize = 20;
/// One section-table entry: kind(4) + pad(4) + offset(8) + len(8).
const SECTION_ENTRY: usize = 24;
/// One root entry: fs_kind(1) + pad(7) + device(8) + generation(8) +
/// mtime(8) + path_off(8) + path_len(4) + pad(4).
const ROOT_ENTRY: usize = 48;
/// The serialized size of a `FileRecord`.
const RECORD_BYTES: usize = std::mem::size_of::<FileRecord>();

/// The snapshot format version this build reads and writes.
pub fn format_version() -> u32 {
    VERSION
}

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
    /// A4: the root directory's mtime at scan time (Unix seconds), for the
    /// non-btrfs staleness watermark. 0 when unavailable.
    ///
    /// On ext4/xfs/f2fs there is no cheap generation counter, so a warm start
    /// compares this watermark against the root's current mtime: a root whose
    /// mtime moved forward while the daemon was down had an entry created,
    /// removed, or renamed directly in it, so its snapshot subtree is stale and
    /// must be rescanned. A root whose mtime is unchanged may still have had
    /// deeper changes, but those are covered by the periodic idle rescan.
    pub mtime: u64,
}

/// How a warm start loads the snapshot (F6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotLoad {
    /// Read the whole file into memory, then decode.
    #[default]
    Read,
    /// Memory-map the file and decode from the mapping, avoiding the
    /// intermediate read buffer.
    Mmap,
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
        // A4: the root's mtime is the non-btrfs staleness watermark. Clamp to
        // 0 for a pre-epoch timestamp, matching the record's mtime handling.
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
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

/// Serialize `index` and its root identities into a v6 image.
///
/// The image is directly mappable: the `RECORDS` section is a bulk copy of the
/// live `FileRecord` array (with `parent` links remapped past tombstones), and
/// the `PATHS`/`ROOT_STRINGS` sections are raw arenas. No per-field
/// serialization is performed.
pub fn encode(index: &Index, identities: &[RootIdentity]) -> Vec<u8> {
    let roots = index.roots();

    // ROOT_STRINGS arena and the ROOTS section.
    let mut root_strings: Vec<u8> = Vec::new();
    let mut roots_section: Vec<u8> = Vec::new();
    put_u32(&mut roots_section, roots.len() as u32);
    for (i, root) in roots.iter().enumerate() {
        let id = identities.get(i).copied().unwrap_or(RootIdentity {
            device: 0,
            generation: 0,
            mtime: 0,
        });
        let path_off = root_strings.len() as u64;
        root_strings.extend_from_slice(root.path.as_bytes());
        put_u8(&mut roots_section, fs_kind_code(root.fs_kind));
        roots_section.extend_from_slice(&[0u8; 7]);
        put_u64(&mut roots_section, id.device);
        put_u64(&mut roots_section, id.generation);
        put_u64(&mut roots_section, id.mtime);
        put_u64(&mut roots_section, path_off);
        put_u32(&mut roots_section, root.path.len() as u32);
        roots_section.extend_from_slice(&[0u8; 4]);
    }

    // RECORDS section: compacted live records with remapped parents. Tombstones
    // are not persisted, so the image stays proportional to the live set.
    let records = index.records();
    let mut remap = vec![u32::MAX; records.len()];
    let mut live = 0u32;
    for (i, rec) in records.iter().enumerate() {
        if !rec.is_deleted() {
            remap[i] = live;
            live += 1;
        }
    }
    let mut records_section: Vec<u8> = Vec::with_capacity(8 + live as usize * RECORD_BYTES);
    put_u32(&mut records_section, live);
    records_section.extend_from_slice(&[0u8; 4]);
    for rec in records.iter() {
        if rec.is_deleted() {
            continue;
        }
        let parent = if rec.parent == u32::MAX {
            u32::MAX
        } else {
            remap[rec.parent as usize]
        };
        let out = FileRecord {
            size: rec.size,
            mtime: rec.mtime,
            parent,
            path_off: rec.path_off,
            path_len: rec.path_len,
            depth: rec.depth,
            flags: rec.flags,
        };
        records_section.extend_from_slice(record_bytes(&out));
    }

    let paths_section = index.paths();

    let sections: [(u32, &[u8]); 4] = [
        (SECTION_ROOTS, roots_section.as_slice()),
        (SECTION_RECORDS, records_section.as_slice()),
        (SECTION_PATHS, paths_section),
        (SECTION_ROOT_STRINGS, root_strings.as_slice()),
    ];
    let section_count = sections.len();
    let header_len = align8(HEADER_FIXED + section_count * SECTION_ENTRY);

    // Section offsets, each 8-byte aligned.
    let mut offsets = [0usize; 4];
    let mut cursor = header_len;
    for (i, (_, data)) in sections.iter().enumerate() {
        offsets[i] = cursor;
        cursor = align8(cursor + data.len());
    }

    let mut out = Vec::with_capacity(cursor + 8);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);
    put_u32(&mut out, header_len as u32);
    put_u32(&mut out, section_count as u32);
    for (i, (kind, data)) in sections.iter().enumerate() {
        put_u32(&mut out, *kind);
        put_u32(&mut out, 0);
        put_u64(&mut out, offsets[i] as u64);
        put_u64(&mut out, data.len() as u64);
    }
    out.resize(header_len, 0);
    for (i, (_, data)) in sections.iter().enumerate() {
        debug_assert_eq!(out.len(), offsets[i]);
        out.extend_from_slice(data);
        out.resize(align8(out.len()), 0);
    }
    let checksum = fnv1a64(&out);
    put_u64(&mut out, checksum);
    out
}

/// Deserialize a snapshot, returning the index and its root identities.
///
/// This materializes an owned [`Index`] from the image; the daemon's evicted
/// path uses [`IndexImage::from_bytes`] directly to avoid the copy.
pub fn decode(bytes: &[u8]) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    let image = IndexImage::from_bytes(bytes)?;
    let identities = image.identities().to_vec();
    Ok((image.to_index(), identities))
}

/// A borrowed view over a v6 snapshot image.
///
/// The image is byte-identical to the on-disk snapshot: `records` and `paths`
/// borrow directly from the mapping (no decode, no rebuild), so loading is an
/// `mmap` plus header validation. `roots` and `identities` are parsed from the
/// small `ROOTS`/`ROOT_STRINGS` sections.
pub struct IndexImage<'a> {
    records: Records<'a>,
    paths: &'a [u8],
    roots: Vec<Root>,
    identities: Vec<RootIdentity>,
    live: usize,
}

/// The record array of an image: borrowed from the mapping when the backing
/// buffer is 8-byte aligned, or an owned copy otherwise (the read path).
enum Records<'a> {
    Borrowed(&'a [FileRecord]),
    Owned(Vec<FileRecord>),
}

impl Records<'_> {
    fn as_slice(&self) -> &[FileRecord] {
        match self {
            Records::Borrowed(s) => s,
            Records::Owned(v) => v,
        }
    }
}

impl<'a> IndexImage<'a> {
    /// Parse and validate a v6 image from `bytes`.
    ///
    /// The record and path arenas are borrowed from `bytes` (no allocation, no
    /// rebuild); only the small `ROOTS`/`ROOT_STRINGS` sections are parsed into
    /// owned vectors.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, SnapshotError> {
        if bytes.len() < HEADER_FIXED + 8 {
            return Err(SnapshotError::Truncated);
        }
        if &bytes[..8] != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let section_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;

        // The checksum covers everything before the trailing 8 bytes.
        let (body, tail) = bytes.split_at(bytes.len() - 8);
        let expected = u64::from_le_bytes(tail.try_into().unwrap());
        let actual = fnv1a64(body);
        if expected != actual {
            return Err(SnapshotError::ChecksumMismatch { expected, actual });
        }

        // Section table.
        let table_end = HEADER_FIXED
            .checked_add(
                section_count
                    .checked_mul(SECTION_ENTRY)
                    .ok_or(SnapshotError::Truncated)?,
            )
            .ok_or(SnapshotError::Truncated)?;
        if table_end > body.len() || header_len < table_end || header_len % 8 != 0 {
            return Err(SnapshotError::Truncated);
        }
        let mut sections: [Option<(usize, usize)>; 4] = [None; 4];
        for i in 0..section_count {
            let base = HEADER_FIXED + i * SECTION_ENTRY;
            let kind = u32::from_le_bytes(body[base..base + 4].try_into().unwrap());
            let offset = u64::from_le_bytes(body[base + 8..base + 16].try_into().unwrap()) as usize;
            let len = u64::from_le_bytes(body[base + 16..base + 24].try_into().unwrap()) as usize;
            let end = offset.checked_add(len).ok_or(SnapshotError::Truncated)?;
            if offset % 8 != 0 || end > body.len() {
                return Err(SnapshotError::Truncated);
            }
            let slot = match kind {
                SECTION_ROOTS => 0,
                SECTION_RECORDS => 1,
                SECTION_PATHS => 2,
                SECTION_ROOT_STRINGS => 3,
                _ => continue,
            };
            sections[slot] = Some((offset, len));
        }
        let (roots_off, roots_len) = sections[0].ok_or(SnapshotError::Truncated)?;
        let (records_off, records_len) = sections[1].ok_or(SnapshotError::Truncated)?;
        let (paths_off, paths_len) = sections[2].ok_or(SnapshotError::Truncated)?;
        let (root_strings_off, root_strings_len) = sections[3].ok_or(SnapshotError::Truncated)?;

        let root_strings = &body[root_strings_off..root_strings_off + root_strings_len];
        let paths = &body[paths_off..paths_off + paths_len];

        // ROOTS section.
        if roots_len < 4 {
            return Err(SnapshotError::Truncated);
        }
        let root_count =
            u32::from_le_bytes(body[roots_off..roots_off + 4].try_into().unwrap()) as usize;
        let roots_body = &body[roots_off + 4..roots_off + roots_len];
        if root_count
            .checked_mul(ROOT_ENTRY)
            .is_none_or(|n| n > roots_body.len())
        {
            return Err(SnapshotError::Truncated);
        }
        let mut roots = Vec::with_capacity(root_count.min(4096));
        let mut identities = Vec::with_capacity(root_count.min(4096));
        for i in 0..root_count {
            let e = &roots_body[i * ROOT_ENTRY..(i + 1) * ROOT_ENTRY];
            let fs_kind = fs_kind_from_code(e[0]);
            let device = u64::from_le_bytes(e[8..16].try_into().unwrap());
            let generation = u64::from_le_bytes(e[16..24].try_into().unwrap());
            let mtime = u64::from_le_bytes(e[24..32].try_into().unwrap());
            let path_off = u64::from_le_bytes(e[32..40].try_into().unwrap()) as usize;
            let path_len = u32::from_le_bytes(e[40..44].try_into().unwrap()) as usize;
            let end = path_off
                .checked_add(path_len)
                .ok_or(SnapshotError::Truncated)?;
            if end > root_strings.len() {
                return Err(SnapshotError::Truncated);
            }
            let path = std::str::from_utf8(&root_strings[path_off..end])
                .map_err(|_| SnapshotError::InvalidUtf8)?
                .to_string();
            roots.push(Root { path, fs_kind });
            identities.push(RootIdentity {
                device,
                generation,
                mtime,
            });
        }

        // RECORDS section.
        if records_len < 8 {
            return Err(SnapshotError::Truncated);
        }
        let record_count =
            u32::from_le_bytes(body[records_off..records_off + 4].try_into().unwrap()) as usize;
        let records_bytes = &body[records_off + 8..records_off + records_len];
        if record_count.checked_mul(RECORD_BYTES) != Some(records_bytes.len()) {
            return Err(SnapshotError::Truncated);
        }
        let records = if records_bytes.as_ptr() as usize % std::mem::align_of::<FileRecord>() == 0 {
            // SAFETY: the slice is 8-byte aligned and its length is an exact
            // multiple of `size_of::<FileRecord>()` (checked above).
            Records::Borrowed(unsafe { records_from_bytes(records_bytes) })
        } else {
            // The read path's buffer is not guaranteed 8-byte aligned; copy.
            let mut v = Vec::with_capacity(record_count);
            for chunk in records_bytes.chunks_exact(RECORD_BYTES) {
                v.push(read_record(chunk));
            }
            Records::Owned(v)
        };

        // Validate every record's path range and count the live records.
        let mut live = 0usize;
        for rec in records.as_slice() {
            let end = rec.path_off as usize + rec.path_len as usize;
            if end > paths.len() {
                return Err(SnapshotError::Truncated);
            }
            if !rec.is_deleted() {
                live += 1;
            }
        }

        Ok(IndexImage {
            records,
            paths,
            roots,
            identities,
            live,
        })
    }

    /// The raw record array.
    pub fn records(&self) -> &[FileRecord] {
        self.records.as_slice()
    }

    /// The path arena.
    pub fn paths(&self) -> &[u8] {
        self.paths
    }

    /// The parsed roots.
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// The root identities, aligned with [`IndexImage::roots`].
    pub fn identities(&self) -> &[RootIdentity] {
        &self.identities
    }

    /// The number of live (non-deleted) records.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// An estimate of the resident bytes held by the image's data. The pages are
    /// file-backed and reclaimable when the image borrows a mapping.
    pub fn memory_bytes(&self) -> u64 {
        (self.records.as_slice().len() * RECORD_BYTES + self.paths.len()) as u64
    }

    /// Run a query against the image via the storage-agnostic linear scan.
    pub fn search(&self, query: &str, limit: usize, offset: usize) -> SearchOutcome {
        crate::index::search_image(self.records.as_slice(), self.paths, query, limit, offset)
    }

    /// Materialize an owned [`Index`] from the image.
    ///
    /// This copies the record and path arenas and rebuilds the derived path map
    /// and candidate index, so it is the expensive path; the daemon uses it only
    /// when it must mutate the loaded image.
    pub fn to_index(&self) -> Index {
        Index::from_parts(
            self.records.as_slice().to_vec(),
            self.paths.to_vec(),
            self.roots.clone(),
        )
    }
}

/// Parse a `FileRecord` from its 20 serialized bytes.
fn read_record(b: &[u8]) -> FileRecord {
    FileRecord {
        size: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        mtime: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        parent: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        path_off: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        path_len: u16::from_le_bytes(b[16..18].try_into().unwrap()),
        depth: b[18],
        flags: b[19],
    }
}

/// Write a snapshot atomically, keeping the previous one as `<path>.prev`.
pub fn save(index: &Index, identities: &[RootIdentity], path: &Path) -> io::Result<()> {
    let bytes = encode(index, identities);
    write_atomic(&bytes, path)
}

/// Permission bits for the snapshot and its `.prev` fallback.
///
/// The snapshot lists the path names and metadata of everything on the indexed
/// filesystems, so it is owner-only. The daemon is its only reader: the CLI and
/// the shell plugin reach the index over the socket or D-Bus, never this file.
const SNAPSHOT_MODE: u32 = 0o600;

/// Write `bytes` to `path` atomically: a temp file, `fsync`, `rename`, and a
/// parent-directory `fsync` so the rename itself is durable. The previous
/// snapshot is kept as `<path>.prev`.
///
/// Callers that may run concurrently (the periodic writer and the shutdown
/// handler) must serialize their calls; the temp path is fixed. If they do not,
/// the second writer fails on the `O_EXCL` in `open_private` rather than writing
/// through the first writer's temp file.
pub fn write_atomic(bytes: &[u8], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = with_suffix(path, ".tmp");
    {
        // The rename below publishes this inode as the snapshot, so the temp file's
        // mode becomes the snapshot's mode. `open_private` creates a *fresh* inode
        // with that mode rather than reopening and tightening whatever was at the
        // temp path: a descriptor opened on a leftover would outlive the `chmod`
        // that followed, and read the snapshot written through it afterwards.
        let mut file = crate::private_file::open_private(&tmp, SNAPSHOT_MODE)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // Keep the previous snapshot as a fallback.
    if path.exists() {
        let prev = with_suffix(path, ".prev");
        // Tighten the outgoing snapshot *before* it becomes the fallback. The rename
        // keeps the inode, so a `chmod` afterwards would publish a complete `.prev`
        // and leave a window in which anyone could still open it -- the same
        // publish-then-tighten mistake as the temp file above, one path over.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(SNAPSHOT_MODE));
        let _ = fs::rename(path, &prev);
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

/// Load a snapshot by memory-mapping the file (F6), falling back to `<path>.prev`
/// and then to a plain read if the mapping is unavailable.
///
/// The mapping is parsed as an [`IndexImage`] and then materialized into an
/// owned [`Index`]; the daemon's evicted path keeps the mapping instead.
pub fn load_mmap(path: &Path) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    match mmap_one(path) {
        Ok(loaded) => Ok(loaded),
        Err(_) => match mmap_one(&with_suffix(path, ".prev")) {
            Ok(loaded) => Ok(loaded),
            Err(_) => load(path),
        },
    }
}

fn mmap_one(path: &Path) -> Result<(Index, Vec<RootIdentity>), SnapshotError> {
    let file = File::open(path)?;
    // SAFETY: snapshots are written atomically (a temp file is renamed into
    // place), so a mapped file is never mutated in place; a concurrent writer
    // publishes a new inode rather than editing the mapped one.
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    let image = IndexImage::from_bytes(&mmap)?;
    let identities = image.identities().to_vec();
    Ok((image.to_index(), identities))
}

/// Build an index, warm-starting from `snapshot_path` when possible.
///
/// Roots whose identity or filesystem kind changed (or that are new) are
/// rescanned; roots that are no longer configured are dropped.
pub fn warm_start(config: &ScanConfig, snapshot_path: &Path) -> WarmStartResult {
    warm_start_with(config, snapshot_path, SnapshotLoad::Read)
}

/// Like [`warm_start`], but selects how the snapshot is loaded (F6).
pub fn warm_start_with(
    config: &ScanConfig,
    snapshot_path: &Path,
    load_mode: SnapshotLoad,
) -> WarmStartResult {
    let roots = resolve_roots(config);

    let loaded = match load_mode {
        SnapshotLoad::Read => load(snapshot_path),
        SnapshotLoad::Mmap => load_mmap(snapshot_path),
    };
    let Ok((mut index, stored_ids)) = loaded else {
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
                mtime: 0,
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

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Round `n` up to the next multiple of 8.
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// View a `FileRecord` as its raw bytes.
fn record_bytes(rec: &FileRecord) -> &[u8] {
    // SAFETY: `FileRecord` is `#[repr(C)]` and exactly 20 bytes with no padding
    // (asserted at compile time), and every bit pattern is a valid `u8`.
    unsafe {
        std::slice::from_raw_parts(
            rec as *const FileRecord as *const u8,
            std::mem::size_of::<FileRecord>(),
        )
    }
}

/// View a byte slice as `[FileRecord]`.
///
/// # Safety
/// `bytes` must be 8-byte aligned and its length an exact multiple of
/// `size_of::<FileRecord>()`.
unsafe fn records_from_bytes(bytes: &[u8]) -> &[FileRecord] {
    debug_assert_eq!(
        bytes.as_ptr() as usize % std::mem::align_of::<FileRecord>(),
        0
    );
    debug_assert_eq!(bytes.len() % std::mem::size_of::<FileRecord>(), 0);
    // SAFETY: the caller guarantees `bytes` is 8-byte aligned and its length an
    // exact multiple of `size_of::<FileRecord>()`.
    unsafe {
        std::slice::from_raw_parts(
            bytes.as_ptr() as *const FileRecord,
            bytes.len() / std::mem::size_of::<FileRecord>(),
        )
    }
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

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn writes_owner_only_snapshot() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("snapshot-mode");
        let path = dir.join("index.snapshot");
        let index = sample_index();

        save(&index, &[], &path).unwrap();
        assert_eq!(mode_of(&path), 0o600, "a fresh snapshot must be owner-only");

        // A snapshot left readable by an older build must not survive the next
        // write, and neither may the `.prev` copy of it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save(&index, &[], &path).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&with_suffix(&path, ".prev")), 0o600);
    }

    #[test]
    fn round_trips_through_encode_decode() {
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 42,
            generation: 99,
            mtime: 0,
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
            mtime: 0,
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
            mtime: 0,
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
            mtime: 0,
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
            mtime: 0,
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

    #[test]
    fn format_version_is_reported() {
        assert_eq!(format_version(), VERSION);
    }

    #[test]
    fn warm_start_finds_a_file_created_while_down_on_non_btrfs() {
        // M17 (A4): on ext4/xfs/f2fs there is no generation counter, so the
        // root's mtime is the staleness watermark. A file created directly in
        // the root while the daemon was down bumps the root's mtime, so the
        // warm start must rescan and find it.
        let dir = temp_dir("warm-stale");
        let data = dir.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("a.txt"), b"x").unwrap();
        let snap = dir.join("index.snapshot");
        let config = ScanConfig::with_roots(vec![data.clone()]);

        let cold = warm_start(&config, &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        // "The daemon is down": a file appears in the root after the snapshot
        // was written. This bumps the root directory's mtime. The mtime is
        // set explicitly because the test runs within the same second the
        // snapshot was taken, and the watermark's granularity is 1 s.
        fs::write(data.join("offline.txt"), b"y").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = File::open(&data).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        drop(f);

        let warm = warm_start(&config, &snap);
        assert_eq!(
            warm.outcome,
            WarmStart::Partial { rescanned: 1 },
            "a root whose mtime moved forward must be rescanned"
        );
        assert!(warm.index.find(&data.join("offline.txt")).is_some());
        assert!(warm.index.find(&data.join("a.txt")).is_some());

        // A second warm start with no further changes is a snapshot hit: the
        // watermark was refreshed by the rescan's identities.
        save(&warm.index, &warm.identities, &snap).unwrap();
        let warm2 = warm_start(&config, &snap);
        assert_eq!(warm2.outcome, WarmStart::Snapshot);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_watermark_survives_a_snapshot_round_trip() {
        // The mtime watermark must be persisted in the ROOTS section and
        // restored by decode, so a warm start compares against the scan-time
        // value rather than 0.
        let dir = temp_dir("warm-watermark");
        let data = dir.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("a.txt"), b"x").unwrap();
        let snap = dir.join("index.snapshot");
        let config = ScanConfig::with_roots(vec![data.clone()]);

        let cold = warm_start(&config, &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        let (_, ids) = load(&snap).unwrap();
        let current = root_identity(&data).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].device, current.device);
        assert_eq!(
            ids[0].mtime, current.mtime,
            "the persisted watermark must match the root's mtime"
        );
        assert!(ids[0].mtime > 0, "the watermark must be a real timestamp");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mmap_load_round_trips() {
        // F6: the mmap load path must decode the same index as the read path.
        let dir = temp_dir("mmap");
        let path = dir.join("index.snapshot");
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 5,
            generation: 2,
            mtime: 0,
        }];
        save(&index, &ids, &path).unwrap();

        let (back, back_ids) = load_mmap(&path).unwrap();
        assert_eq!(back.len(), index.len());
        assert_eq!(back_ids, ids);
        assert_eq!(back.search("report", 10, 0).hits.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mmap_load_falls_back_to_prev() {
        let dir = temp_dir("mmap-prev");
        let path = dir.join("index.snapshot");
        let ids = vec![RootIdentity {
            device: 1,
            generation: 0,
            mtime: 0,
        }];
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/a.txt"), false, 1, 0);
        save(&index, &ids, &path).unwrap();
        index.insert(Path::new("/b.txt"), false, 2, 0);
        save(&index, &ids, &path).unwrap();

        // Corrupt the primary; the mmap loader must fall back to `.prev`.
        fs::write(&path, b"garbage").unwrap();
        let (loaded, _) = load_mmap(&path).unwrap();
        assert!(loaded.find(Path::new("/a.txt")).is_some());
        assert!(loaded.find(Path::new("/b.txt")).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_start_with_mmap_matches_read() {
        let dir = temp_dir("warm-mmap");
        let data = dir.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("a.txt"), b"x").unwrap();
        let snap = dir.join("index.snapshot");
        let config = ScanConfig::with_roots(vec![data.clone()]);

        let cold = warm_start(&config, &snap);
        save(&cold.index, &cold.identities, &snap).unwrap();

        let read = warm_start_with(&config, &snap, SnapshotLoad::Read);
        let mmap = warm_start_with(&config, &snap, SnapshotLoad::Mmap);
        assert_eq!(read.outcome, WarmStart::Snapshot);
        assert_eq!(mmap.outcome, WarmStart::Snapshot);
        assert_eq!(read.index.len(), mmap.index.len());
        assert!(mmap.index.find(&data.join("a.txt")).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_round_trips_and_searches() {
        let index = sample_index();
        let ids = vec![RootIdentity {
            device: 42,
            generation: 99,
            mtime: 0,
        }];
        let bytes = encode(&index, &ids);
        let image = IndexImage::from_bytes(&bytes).unwrap();

        assert_eq!(image.len(), index.len());
        assert_eq!(image.roots().len(), 1);
        assert_eq!(image.roots()[0].fs_kind, FsKind::Btrfs);
        assert_eq!(image.identities(), ids.as_slice());

        let out = image.search("report", 10, 0);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].path, "/home/sam/report.pdf");
        assert_eq!(out.hits[0].size, 1234);
        assert_eq!(out.hits[0].mtime, 42);
    }

    #[test]
    fn image_search_matches_owned_index() {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        for i in 0..500 {
            index.insert(
                Path::new(&format!("/home/sam/dir-{}/file-{i}.txt", i % 7)),
                false,
                i as u64,
                i as i64,
            );
        }
        index.insert(
            Path::new("/home/sam/Documents/report-2024.pdf"),
            false,
            1,
            0,
        );
        // The image has no candidate index, so compare against the owned index
        // with the candidate index disabled (the linear-scan path).
        index.set_candidate_enabled(false);

        let bytes = encode(&index, &[]);
        let image = IndexImage::from_bytes(&bytes).unwrap();

        for q in [
            "report",
            "file-1",
            "dir-3",
            "txt",
            "documents",
            "nomatch-xyz",
        ] {
            let owned = index.search(q, 20, 0);
            let viewed = image.search(q, 20, 0);
            assert_eq!(owned.total, viewed.total, "total mismatch for {q:?}");
            assert_eq!(owned.hits, viewed.hits, "hits mismatch for {q:?}");
        }
    }

    #[test]
    fn image_rejects_corruption() {
        let index = sample_index();
        let good = encode(&index, &[]);

        // Bad magic.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(
            IndexImage::from_bytes(&bad),
            Err(SnapshotError::BadMagic)
        ));

        // Truncated header.
        assert!(matches!(
            IndexImage::from_bytes(&good[..10]),
            Err(SnapshotError::Truncated)
        ));

        // Flipped payload byte -> checksum mismatch.
        let mut flipped = good.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xff;
        assert!(matches!(
            IndexImage::from_bytes(&flipped),
            Err(SnapshotError::ChecksumMismatch { .. })
        ));

        // A section table that points out of bounds is rejected even with a
        // valid checksum: rewrite the first section's offset, then fix the
        // checksum so the section-table check is reached.
        let mut oob = good.clone();
        let off = HEADER_FIXED + 8;
        oob[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let body_len = oob.len() - 8;
        let checksum = fnv1a64(&oob[..body_len]);
        oob[body_len..].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            IndexImage::from_bytes(&oob),
            Err(SnapshotError::Truncated)
        ));
    }

    #[test]
    fn image_memory_bytes_reports_data() {
        let index = sample_index();
        let bytes = encode(&index, &[]);
        let image = IndexImage::from_bytes(&bytes).unwrap();
        let expected = (std::mem::size_of_val(image.records()) + image.paths().len()) as u64;
        assert_eq!(image.memory_bytes(), expected);
    }
}

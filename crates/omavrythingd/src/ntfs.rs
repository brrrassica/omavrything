//! NTFS partition mounting, auto-mount policy, and MFT-based indexing.
//!
//! Mounting prefers udisks2 (so the desktop sees the partition) and falls back
//! to a private read-only `ntfs3` mount. Indexing reads the MFT directly from
//! the raw device via `omavrything-ntfs`, so a partition can be indexed without
//! being mounted.
//!
//! M18 (mount lifecycle): the daemon records which mounts it created in an
//! ownership state file, unmounts only those on shutdown, reconciles the state
//! on restart, and drops a mount's index entries when the mount disappears
//! (re-indexing when it returns).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omavrything_core::index::{FsKind, Index};
use omavrything_ntfs::{NtfsEntry, NtfsPartition, NtfsVolume};
use tracing::{debug, info, warn};

use crate::manager::IndexManager;

/// The private mount root used when udisks2 is unavailable.
pub(crate) const PRIVATE_MOUNT_ROOT: &str = "/run/omavrything/ntfs";

/// B5: every mount is read-only and hardened against setuid, device nodes, and
/// execution. The daemon only reads the MFT, so none of these are needed.
pub(crate) const MOUNT_OPTIONS: &str = "ro,nosuid,nodev,noexec";

/// B5: a mount or udisks2 call that hangs must not wedge the daemon.
pub(crate) const MOUNT_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const UDISKS_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// B7: serialize policy-file writes in-process. Callers already hold the
/// `Mutex<NtfsPolicy>`; this lock also covers a save reached through a
/// different policy instance (e.g. startup reconciliation).
static POLICY_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// M18 (B2): the mount-ownership state file, next to the auto-mount policy.
/// One `mount_point<TAB>owned` line per tracked mount; `owned` is `1` for a
/// mount the daemon created (unmounted on shutdown) and `0` for a pre-existing
/// user/udisks2 mount (never unmounted).
const OWNED_FILE_NAME: &str = "mounts.state";

/// The error prefix returned when a mount is writable and MFT indexing is
/// refused (B12). This is an expected, non-fatal outcome — the generic
/// filesystem walker still indexes the mount — so callers log it at `info`
/// rather than `error`. Shared so the producer and the caller cannot drift.
pub const WRITABLE_MOUNT_ERR: &str = "refusing to index writable mount";

/// Mount an NTFS device read-only, preferring udisks2 and falling back to a
/// private `ntfs3` mount. Returns the mount point.
///
/// A device that is already mounted (e.g. by udisks2 at login) reuses its
/// existing mount point rather than attempting a second mount. A pre-existing
/// mount that is writable is remounted read-only (B12): the daemon never
/// indexes a writable mount, so the MFT read cannot set the volume's dirty bit.
pub fn mount_readonly(device: &Path, uuid: Option<&str>) -> Result<PathBuf, String> {
    if let Some(mount_point) = existing_mount_point(device) {
        // Diagnostics: a pre-existing mount is *not* daemon-created, and its
        // read-write state is whatever the desktop (udisks2) chose. Log it so
        // the mount-ownership and ro/rw questions can be answered from logs.
        let state = mount_state(device);
        info!(
            device = %device.display(),
            mount_point = %mount_point.display(),
            fs_type = state.as_ref().map(|s| s.0.as_str()).unwrap_or("?"),
            options = state.as_ref().map(|s| s.1.as_str()).unwrap_or("?"),
            "ntfs: reusing pre-existing mount (not daemon-created)"
        );
        // B12: the daemon only indexes read-only mounts. A pre-existing mount
        // (e.g. a udisks2 auto-mount at login) is usually writable; remount it
        // read-only so the MFT read cannot set the volume's dirty bit. The
        // remount is kept: the daemon's model is read-only mounts, and a
        // writable mount is never indexed.
        if mount_is_readonly(&mount_point) == Some(false) {
            remount_readonly(&mount_point)?;
        }
        return Ok(mount_point);
    }
    // B3: prefer the udisks2 D-Bus API. It returns the mount point directly,
    // so no English output is parsed. A system service may be denied by
    // udisks2/polkit; fall back to `udisksctl`, then to the private ntfs3
    // mount.
    match crate::udisks::mount(device, Some(MOUNT_OPTIONS)) {
        Ok(mount_point) => {
            info!(
                device = %device.display(),
                mount_point = %mount_point.display(),
                "ntfs: udisks2 D-Bus mount succeeded"
            );
            return Ok(mount_point);
        }
        Err(err) => {
            debug!(
                device = %device.display(),
                error = %err,
                "ntfs: udisks2 D-Bus mount failed; falling back"
            );
        }
    }
    if let Some(mount_point) = mount_via_udisks(device) {
        return Ok(mount_point);
    }
    mount_via_ntfs3(device, uuid)
}

/// Remount `mount_point` read-only (B12). Requires `CAP_SYS_ADMIN`, which the
/// systemd unit grants. The remount is kept: the daemon never indexes a
/// writable mount, so a pre-existing writable mount (a udisks2 auto-mount) is
/// made read-only for the daemon's lifetime.
fn remount_readonly(mount_point: &Path) -> Result<(), String> {
    info!(
        mount_point = %mount_point.display(),
        command = "mount -o remount,ro,nosuid,nodev,noexec <mount_point>",
        "ntfs: remounting a writable mount read-only"
    );
    let mount_point_str = mount_point.to_string_lossy();
    let output = run_command_timeout(
        "mount",
        &["-o", "remount,ro,nosuid,nodev,noexec", &mount_point_str],
        MOUNT_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            mount_point = %mount_point.display(),
            stderr = %stderr,
            "ntfs: remount read-only failed"
        );
        return Err(format!(
            "could not remount {} read-only: {stderr}",
            mount_point.display()
        ));
    }
    Ok(())
}

/// The filesystem type and mount options for `device` from `/proc/mounts`, as
/// `(fs_type, options)`. Used only for diagnostics.
fn mount_state(device: &Path) -> Option<(String, String)> {
    let mounts = mount_table().ok()?;
    let dev = device.to_string_lossy();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        let _target = fields.next()?;
        let fs_type = fields.next()?;
        let options = fields.next().unwrap_or("");
        if source == dev {
            return Some((fs_type.to_string(), options.to_string()));
        }
    }
    None
}

/// The mount points of daemon-created private NTFS mounts (under
/// [`PRIVATE_MOUNT_ROOT`]) that are currently live, from `/proc/mounts`.
pub fn private_mounts() -> Vec<PathBuf> {
    mount_table()
        .map(|text| private_mounts_from(&text))
        .unwrap_or_default()
}

/// Parse the private mount points out of a `/proc/mounts` body.
fn private_mounts_from(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _source = fields.next()?;
            let target = fields.next()?;
            let target = PathBuf::from(unescape_mount(target));
            target.starts_with(PRIVATE_MOUNT_ROOT).then_some(target)
        })
        .collect()
}

/// Whether the mount at `mount_point` is mounted read-only, from
/// `/proc/mounts`. `None` when the mount point is not currently mounted.
///
/// Used to enforce the read-only invariant (B12): a writable mount is never
/// indexed, so a forced close or power loss cannot set a dirty bit or lose
/// data through the daemon.
pub fn mount_is_readonly(mount_point: &Path) -> Option<bool> {
    mount_table()
        .ok()
        .and_then(|text| mount_is_readonly_from(&text, mount_point))
}

/// Parse the read-only state of `mount_point` out of a `/proc/mounts` body.
fn mount_is_readonly_from(text: &str, mount_point: &Path) -> Option<bool> {
    let target = mount_point.to_string_lossy();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let _source = fields.next()?;
        let mp = fields.next()?;
        let _fs_type = fields.next()?;
        let options = fields.next().unwrap_or("");
        if unescape_mount(mp) == target {
            return Some(readonly_from_options(options));
        }
    }
    None
}

/// Whether a `/proc/mounts` option list marks the mount read-only.
fn readonly_from_options(options: &str) -> bool {
    options.split(',').any(|o| o == "ro")
}

/// The mount point of `device` if it is already mounted, from `/proc/mounts`.
pub fn existing_mount_point(device: &Path) -> Option<PathBuf> {
    let mounts = mount_table().ok()?;
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

/// The daemon's mount table (`/proc/mounts`).
///
/// The daemon's mount namespace is a slave of the host's, so mounts the desktop
/// creates (udisks2) propagate down into it, and mounts the daemon creates are
/// visible here too. `/proc/1/mounts` (the host table) is not readable by the
/// unprivileged daemon user and would hide daemon-created mounts anyway, so
/// mounting happens in the daemon's own namespace.
fn mount_table() -> io::Result<String> {
    std::fs::read_to_string("/proc/mounts")
}

/// Run `prog` with `args`, capturing its output, and kill it if it does not
/// finish within `timeout` (B5). A hung `mount`/`udisksctl` must not wedge the
/// daemon's startup or hotplug thread.
pub(crate) fn run_command_timeout(
    prog: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<Output> {
    let mut child = Command::new(prog)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{prog} timed out after {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
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
    let device_str = device.to_string_lossy();
    info!(
        device = %device.display(),
        command = "udisksctl mount --no-user-interaction -b <device> -o ro,nosuid,nodev,noexec",
        "ntfs: attempting udisks2 mount"
    );
    let output = run_command_timeout(
        "udisksctl",
        &[
            "mount",
            "--no-user-interaction",
            "-b",
            &device_str,
            "-o",
            MOUNT_OPTIONS,
        ],
        UDISKS_TIMEOUT,
    )
    .ok()?;
    if !output.status.success() {
        warn!(
            device = %device.display(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "ntfs: udisksctl mount failed"
        );
        return None;
    }
    // B3: prefer the kernel mount table over parsing English output. The
    // `udisksctl` text is only a fallback for a mount that has not appeared in
    // `/proc/mounts` yet.
    let mount_point = existing_mount_point(device)
        .or_else(|| parse_udisks_mount_point(&String::from_utf8_lossy(&output.stdout)));
    if let Some(ref mp) = mount_point {
        let state = mount_state(device);
        info!(
            device = %device.display(),
            mount_point = %mp.display(),
            fs_type = state.as_ref().map(|s| s.0.as_str()).unwrap_or("?"),
            options = state.as_ref().map(|s| s.1.as_str()).unwrap_or("?"),
            "ntfs: udisks2 mount result"
        );
    }
    mount_point
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

/// B6: a stable, collision-free private mount-point name. Prefer the UUID;
/// fall back to the device basename (e.g. `sda1`) when the volume has no UUID,
/// so two UUID-less volumes never share the constant `volume` mount point.
fn private_mount_name(device: &Path, uuid: Option<&str>) -> String {
    let raw = uuid
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            device
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "volume".to_string());
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "volume".to_string()
    } else {
        sanitized
    }
}

/// Mount via the kernel `ntfs3` driver at a private read-only mount point.
fn mount_via_ntfs3(device: &Path, uuid: Option<&str>) -> Result<PathBuf, String> {
    let name = private_mount_name(device, uuid);
    let mount_point = PathBuf::from(PRIVATE_MOUNT_ROOT).join(name);
    std::fs::create_dir_all(&mount_point).map_err(|e| e.to_string())?;
    let device_str = device.to_string_lossy();
    let mount_point_str = mount_point.to_string_lossy();
    info!(
        device = %device.display(),
        mount_point = %mount_point.display(),
        command = "mount -t ntfs3 -o ro,nosuid,nodev,noexec <device> <mount_point>",
        "ntfs: attempting private ntfs3 mount (daemon-created)"
    );
    let output = run_command_timeout(
        "mount",
        &[
            "-t",
            "ntfs3",
            "-o",
            MOUNT_OPTIONS,
            &device_str,
            &mount_point_str,
        ],
        MOUNT_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            device = %device.display(),
            mount_point = %mount_point.display(),
            stderr = %stderr,
            "ntfs: private ntfs3 mount failed"
        );
        return Err(stderr);
    }
    let state = mount_state(device);
    info!(
        device = %device.display(),
        mount_point = %mount_point.display(),
        fs_type = state.as_ref().map(|s| s.0.as_str()).unwrap_or("?"),
        options = state.as_ref().map(|s| s.1.as_str()).unwrap_or("?"),
        "ntfs: private ntfs3 mount result (daemon-created; unmounted on shutdown)"
    );
    Ok(mount_point)
}

/// How many times a busy unmount is retried before the lazy fallback, and how
/// long to wait between attempts (B13).
const BUSY_UNMOUNT_RETRIES: u32 = 3;
const BUSY_UNMOUNT_BACKOFF: Duration = Duration::from_millis(200);

/// Whether `mount_point` is still a live mount under [`PRIVATE_MOUNT_ROOT`].
fn still_mounted(mount_point: &Path) -> bool {
    private_mounts().iter().any(|m| m == mount_point)
}

/// Unmount one private mount point, retrying a bounded number of times when
/// the mount is busy, then falling back to a lazy (`umount -l`) unmount (B13).
/// Returns whether the mount was released; a mount that survives even the lazy
/// unmount is logged with its mount point and reported (never fatal, B14).
fn unmount_mount_point(mount_point: &Path) -> bool {
    let mount_point_str = mount_point.to_string_lossy();
    for attempt in 1..=BUSY_UNMOUNT_RETRIES {
        let ok = run_command_timeout("umount", &[&mount_point_str], UNMOUNT_TIMEOUT)
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok && !still_mounted(mount_point) {
            return true;
        }
        debug!(
            mount_point = %mount_point.display(),
            attempt,
            "ntfs: umount failed or the mount is still live; retrying"
        );
        std::thread::sleep(BUSY_UNMOUNT_BACKOFF);
    }
    let lazy = run_command_timeout("umount", &["-l", &mount_point_str], UNMOUNT_TIMEOUT)
        .map(|o| o.status.success())
        .unwrap_or(false);
    if lazy && !still_mounted(mount_point) {
        info!(
            mount_point = %mount_point.display(),
            "ntfs: lazy unmount released the busy mount"
        );
        return true;
    }
    warn!(
        mount_point = %mount_point.display(),
        "ntfs: mount could not be released even with a lazy unmount; leaving it in place (non-fatal)"
    );
    false
}

/// Unmount every live daemon-created private mount. Used on shutdown (B9) and
/// by startup reconciliation (B10). Each unmount is bounded by the retry loop
/// and the lazy fallback (B13); a failure is logged, never fatal (B14).
///
/// M18 (B1): only mounts the daemon created are unmounted. A private mount
/// under [`PRIVATE_MOUNT_ROOT`] is daemon-created by construction; a
/// pre-existing user/udisks2 mount (e.g. under `/run/media`) is never in that
/// set, so it is never touched. Released mounts are forgotten from the
/// ownership state (B2).
pub fn unmount_all_private() {
    let mounts = private_mounts();
    if mounts.is_empty() {
        return;
    }
    let mut ownership = MountOwnership::load(default_ownership_path());
    let owned = daemon_owned_mounts(mounts, &ownership);
    if owned.is_empty() {
        return;
    }
    info!(
        mounts = ?owned,
        "ntfs: unmounting daemon-created private mounts"
    );
    for mount_point in &owned {
        if unmount_mount_point(mount_point) {
            let _ = ownership.forget(mount_point);
        }
    }
}

/// B1: select only the mounts the daemon owns. A private mount under
/// [`PRIVATE_MOUNT_ROOT`] is daemon-created by construction; the ownership
/// state is consulted so a mount recorded as pre-existing is never selected.
fn daemon_owned_mounts(mounts: Vec<PathBuf>, ownership: &MountOwnership) -> Vec<PathBuf> {
    mounts
        .into_iter()
        .filter(|mp| mp.starts_with(PRIVATE_MOUNT_ROOT) || ownership.is_owned(mp))
        .collect()
}

/// M18 (B2): the persisted mount-ownership state.
///
/// Maps mount point -> owned flag. A mount the daemon created (`owned`) is
/// unmounted on shutdown and reconciled on restart; a pre-existing user or
/// udisks2 mount (`!owned`) is never unmounted. The state is written
/// atomically (temp file + rename) so a crash mid-write leaves the previous
/// state intact.
#[derive(Debug, Default)]
pub struct MountOwnership {
    path: PathBuf,
    entries: std::collections::HashMap<PathBuf, bool>,
}

impl MountOwnership {
    /// Load the ownership state from `path`. A missing or unreadable file is
    /// an empty state (nothing is owned yet).
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .filter_map(|line| {
                        let (mp, owned) = line.split_once('\t')?;
                        Some((PathBuf::from(mp), owned == "1"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    /// Whether `mount_point` is tracked as daemon-owned.
    pub fn is_owned(&self, mount_point: &Path) -> bool {
        self.entries.get(mount_point).copied().unwrap_or(false)
    }

    /// The tracked mount points.
    pub fn mount_points(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.entries.keys().cloned().collect();
        v.sort();
        v
    }

    /// The number of tracked mounts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the state is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record `mount_point` with `owned`, pruning entries for mount points
    /// that are no longer live, and persist atomically.
    pub fn record(&mut self, mount_point: &Path, owned: bool) -> io::Result<()> {
        self.entries.insert(mount_point.to_path_buf(), owned);
        self.save()
    }

    /// Drop the entry for `mount_point` and persist.
    pub fn forget(&mut self, mount_point: &Path) -> io::Result<()> {
        if self.entries.remove(mount_point).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// Prune entries whose mount point is no longer live in `/proc/mounts`
    /// (a mount that disappeared while the daemon was down) and persist.
    pub fn reconcile(&mut self) -> io::Result<()> {
        let live: HashSet<PathBuf> = private_mounts()
            .into_iter()
            .chain(existing_mount_points_anywhere())
            .collect();
        let before = self.entries.len();
        self.entries.retain(|mp, _| live.contains(mp));
        if self.entries.len() != before {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|(mp, owned)| format!("{}\t{}", mp.display(), if *owned { "1" } else { "0" }))
            .collect();
        lines.sort();
        let mut body = lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        // Atomic write: a crash mid-write leaves the previous state intact.
        let tmp = self.path.with_extension("state.tmp");
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// Every mount point currently live in `/proc/mounts`, not just the private
/// ones. Used by [`MountOwnership::reconcile`] so pre-existing (udisks2) mounts
/// stay tracked.
fn existing_mount_points_anywhere() -> Vec<PathBuf> {
    let Ok(mounts) = mount_table() else {
        return Vec::new();
    };
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _source = fields.next()?;
            let target = fields.next()?;
            Some(PathBuf::from(unescape_mount(target)))
        })
        .collect()
}

/// The default ownership-state path, next to the auto-mount policy.
pub fn default_ownership_path() -> PathBuf {
    let mut base = default_policy_path();
    base.set_file_name(OWNED_FILE_NAME);
    base
}

/// M18 (B8): drop the index entries of a mount whose mount point is no longer
/// live. Returns whether entries were dropped.
pub fn drop_mount_entries(index: &IndexManager, mount_point: &Path) -> bool {
    let still_there = existing_mount_points_anywhere()
        .iter()
        .any(|mp| mp == mount_point);
    if still_there {
        return false;
    }
    let dropped = index.with_index_mut(|idx| idx.remove(mount_point));
    if dropped {
        info!(
            mount_point = %mount_point.display(),
            "ntfs: mount disappeared; index entries dropped"
        );
    }
    dropped
}

/// Reconcile the private-mount root on startup: unmount stale daemon-created
/// mounts left behind by a `SIGKILL` or crash, before indexing (B10). Called
/// on the NTFS startup thread, so it is non-blocking for the rest of startup,
/// and a failed unmount is non-fatal (B14): a stuck mount cannot put the
/// daemon into a restart loop.
pub fn reconcile_stale_mounts() {
    let stale = private_mounts();
    if stale.is_empty() {
        return;
    }
    warn!(
        mounts = ?stale,
        "ntfs: stale daemon-created mounts from a previous run; unmounting"
    );
    unmount_all_private();
}

/// B4: react to udev block-device events and reconcile the NTFS mounts.
///
/// The daemon shares the host mount namespace, so a udisks2 mount made by the
/// desktop and a private `ntfs3` mount made by the daemon are both visible in
/// `/proc/mounts`. A udev netlink monitor wakes this pass on a block-device
/// add/remove/change; a 30 s timeout is a fallback for a missed event. If the
/// udev socket cannot be opened, the pass falls back to a 2 s poll.
pub fn run_hotplug_monitor(
    index: Arc<IndexManager>,
    policy: Arc<Mutex<NtfsPolicy>>,
    ownership: Arc<Mutex<MountOwnership>>,
) {
    // Only one monitor may run, even if NTFS is toggled on more than once.
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        debug!("ntfs: hotplug monitor already running");
        return;
    }
    const POLL: Duration = Duration::from_secs(2);
    const FALLBACK: Duration = Duration::from_secs(30);
    const RETRY: Duration = Duration::from_secs(60);
    let events = crate::udev::monitor().ok();
    if events.is_none() {
        warn!("ntfs: udev monitor unavailable; falling back to mount-table polling");
    }
    let mut retry_at: HashMap<PathBuf, Instant> = HashMap::new();
    loop {
        // Drop entries for tracked mounts that vanished while the daemon ran.
        let tracked = {
            let o = ownership.lock().unwrap_or_else(|e| e.into_inner());
            o.mount_points()
        };
        for mount_point in tracked {
            if mount_point_is_live(&mount_point) {
                continue;
            }
            drop_mount_entries(index.as_ref(), &mount_point);
            let mut o = ownership.lock().unwrap_or_else(|e| e.into_inner());
            let _ = o.forget(&mount_point);
        }

        // Mount and index a remembered partition that appeared.
        for part in omavrything_ntfs::list_ntfs_partitions() {
            let Some(mount_point) = existing_mount_point(&part.device) else {
                continue;
            };
            if index
                .roots()
                .iter()
                .any(|root| Path::new(&root.path) == mount_point.as_path())
            {
                continue;
            }
            let known = part
                .uuid
                .as_deref()
                .map(|uuid| {
                    policy
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_known(uuid)
                })
                .unwrap_or(false);
            if !known {
                continue;
            }
            if retry_at
                .get(&part.device)
                .is_some_and(|deadline| *deadline > Instant::now())
            {
                continue;
            }
            match mount_index_and_remember_with_ownership(
                index.as_ref(),
                policy.as_ref(),
                ownership.as_ref(),
                &part,
            ) {
                Ok(mount_point) => {
                    retry_at.remove(&part.device);
                    info!(
                        device = %part.device.display(),
                        mount_point = %mount_point.display(),
                        "ntfs: hotplug mount indexed"
                    );
                }
                Err(err) => {
                    warn!(
                        device = %part.device.display(),
                        error = %err,
                        "ntfs: hotplug mount failed; backing off"
                    );
                    retry_at.insert(part.device.clone(), Instant::now() + RETRY);
                }
            }
        }
        match &events {
            Some(rx) => crate::udev::wait_for_block_event(rx, FALLBACK),
            None => std::thread::sleep(POLL),
        }
    }
}

/// Whether `mount_point` is currently live in `/proc/mounts`.
fn mount_point_is_live(mount_point: &Path) -> bool {
    existing_mount_points_anywhere()
        .iter()
        .any(|live| live == mount_point)
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
    // Diagnostics: the `$Volume` dirty flag is the state in which the kernel
    // `ntfs3` driver replays the journal on mount. Log it before the MFT read
    // so a dirty volume is visible in the logs. This is a read-only probe.
    match volume.volume_flags() {
        Ok(Some(flags)) => info!(
            device = %device.display(),
            flags = format!("0x{flags:04x}"),
            dirty = flags & omavrything_ntfs::mft::VOLUME_DIRTY != 0,
            "ntfs: volume flags before MFT read"
        ),
        Ok(None) => debug!(
            device = %device.display(),
            "ntfs: volume flags unavailable (no $Volume record)"
        ),
        Err(err) => warn!(
            device = %device.display(),
            error = %err,
            "ntfs: could not read volume flags"
        ),
    }
    volume.enumerate().map_err(|e| e.to_string())
}

/// A7: validate an NTFS `entry.path` before joining it onto the mount point.
/// The path comes from attacker-controlled on-disk MFT structures, so a crafted
/// record must not be able to escape the mount point: reject absolute paths,
/// `..` components, and Windows drive/UNC forms.
fn is_safe_rel_path(path: &str) -> bool {
    if path.is_empty() {
        return true; // the volume root itself
    }
    let bytes = path.as_bytes();
    if bytes[0] == b'/' || bytes[0] == b'\\' {
        return false;
    }
    // A Windows drive form (`C:\…`) or a UNC path (`\\server\…`).
    if bytes.len() >= 2 && bytes[1] == b':' {
        return false;
    }
    !path.split(['/', '\\']).any(|c| c == ".." || c == ".")
}

/// Insert pre-enumerated NTFS entries into `index` under `mount_point`. Performs
/// no device I/O, so the caller can hold the index write lock for the duration.
/// Entries whose path fails the A7 validation are skipped. Returns the number
/// of entries added.
pub fn apply_entries(index: &mut Index, mount_point: &Path, entries: &[NtfsEntry]) -> usize {
    index.add_root(mount_point, FsKind::Ntfs);
    let mut added = 0usize;
    for entry in entries {
        if !is_safe_rel_path(&entry.path) {
            warn!(
                path = %entry.path,
                "ntfs: skipping entry with an unsafe MFT path"
            );
            continue;
        }
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
    index: &IndexManager,
    policy: &Mutex<NtfsPolicy>,
    partition: &NtfsPartition,
) -> Result<PathBuf, String> {
    mount_index_and_remember_with_ownership(
        index,
        policy,
        &Mutex::new(MountOwnership::load(default_ownership_path())),
        partition,
    )
}

/// Like [`mount_index_and_remember`], but records the mount's ownership in
/// `ownership` (M18, B2): a mount the daemon created is marked owned (unmounted
/// on shutdown); a pre-existing mount is marked not-owned (never unmounted).
pub fn mount_index_and_remember_with_ownership(
    index: &IndexManager,
    policy: &Mutex<NtfsPolicy>,
    ownership: &Mutex<MountOwnership>,
    partition: &NtfsPartition,
) -> Result<PathBuf, String> {
    let mount_point = mount_readonly(&partition.device, partition.uuid.as_deref())?;
    // B12: never index through a writable mount. A forced close or power loss
    // on a writable mount can set the volume's dirty bit or lose data, so a
    // mount that is not verifiably read-only is refused.
    match mount_is_readonly(&mount_point) {
        Some(true) => {}
        Some(false) => {
            return Err(format!("{WRITABLE_MOUNT_ERR} {}", mount_point.display()));
        }
        None => {
            return Err(format!(
                "mount {} vanished before the read-only check",
                mount_point.display()
            ));
        }
    }
    // M18 (B2): record whether this mount is daemon-owned. A private mount
    // under PRIVATE_MOUNT_ROOT was created by the daemon; anything else
    // (udisks2, a pre-existing mount) is not ours to unmount.
    let owned = mount_point.starts_with(PRIVATE_MOUNT_ROOT);
    {
        let mut o = ownership.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = o.record(&mount_point, owned) {
            warn!(error = %e, "ntfs: could not persist mount ownership");
        }
    }
    info!(
        device = %partition.device.display(),
        mount_point = %mount_point.display(),
        owned,
        "ntfs: mount event (daemon-created private mounts are unmounted on shutdown)"
    );
    // Read and parse the MFT outside the index lock: on a multi-million-record
    // volume this takes seconds, and holding the write lock for it would stall
    // every search for the duration. The guard reports "indexing" to the
    // settings UI for the duration of the read.
    let entries = {
        let _indexing = crate::volumes::IndexingGuard::new(&partition.device);
        enumerate_device(&partition.device)?
    };
    index.with_index_mut(|idx| apply_entries(idx, &mount_point, &entries));
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
        // B7: serialize writers and write atomically (temp file + rename), so a
        // crash mid-write leaves the previous policy intact.
        let _guard = POLICY_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text: Vec<&str> = self.known.iter().map(String::as_str).collect();
        text.sort_unstable();
        let mut body = text.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        let tmp = self.path.with_extension("state.tmp");
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, &self.path)
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
    fn unsafe_mft_paths_are_rejected() {
        // Absolute and Windows-absolute forms cannot escape via join, but are
        // rejected anyway so nothing attacker-shaped is ever joined.
        assert!(!is_safe_rel_path("/etc/passwd"));
        assert!(!is_safe_rel_path("\\Windows\\system32"));
        assert!(!is_safe_rel_path("C:\\Windows"));
        assert!(!is_safe_rel_path("\\\\server\\share"));
        // `..` traversal, in both separator styles.
        assert!(!is_safe_rel_path("../escape"));
        assert!(!is_safe_rel_path("sub/../../escape"));
        assert!(!is_safe_rel_path("sub\\..\\..\\escape"));
        // A single `.` component is likewise never legitimate in an MFT path.
        assert!(!is_safe_rel_path("./x"));
        // Legitimate relative paths pass.
        assert!(is_safe_rel_path("hello.txt"));
        assert!(is_safe_rel_path("subdir/world.txt"));
        assert!(is_safe_rel_path("deep/a/b/c.txt"));
        assert!(is_safe_rel_path(""));
    }

    #[test]
    fn a_crafted_mft_path_cannot_escape_the_mount_point() {
        let mut index = Index::new();
        let mount_point = Path::new("/mnt/ntfs");
        let entries = vec![
            NtfsEntry {
                path: "../../etc/passwd".to_string(),
                is_dir: false,
                size: 1,
                mtime: 0,
            },
            NtfsEntry {
                path: "/etc/shadow".to_string(),
                is_dir: false,
                size: 1,
                mtime: 0,
            },
            NtfsEntry {
                path: "safe.txt".to_string(),
                is_dir: false,
                size: 1,
                mtime: 0,
            },
        ];
        let added = apply_entries(&mut index, mount_point, &entries);
        assert_eq!(added, 1);
        assert!(index.find(Path::new("/mnt/ntfs/safe.txt")).is_some());
        // Nothing outside the mount point was indexed.
        assert!(index.find(Path::new("/etc/passwd")).is_none());
        assert!(index.find(Path::new("/etc/shadow")).is_none());
        assert!(index.find(Path::new("/mnt/etc/passwd")).is_none());
    }

    #[test]
    fn readonly_from_options_matches_ro_only() {
        assert!(readonly_from_options("ro,relatime"));
        assert!(readonly_from_options("relatime,ro"));
        assert!(!readonly_from_options("rw,relatime"));
        assert!(!readonly_from_options("rw,nosuid,nodev,noexec"));
        // A prefix like "roxx" must not count as `ro`.
        assert!(!readonly_from_options("roxx,rw"));
    }

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
    fn ownership_records_loads_and_reconciles() {
        // M18 (B2): the ownership state persists owned vs pre-existing mounts,
        // prunes entries for mounts that are no longer live, and never touches
        // a pre-existing mount's entry.
        let dir = std::env::temp_dir().join(format!("omavrything-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mounts.state");

        // Paths that are guaranteed not to be live mounts in any environment.
        let owned_mp = Path::new("/run/omavrything/ntfs/omavrything-test-vol");
        let preexisting_mp = Path::new("/omavrything-test-preexisting-mount");

        let mut o = MountOwnership::load(path.clone());
        assert!(o.is_empty());
        o.record(owned_mp, true).unwrap();
        o.record(preexisting_mp, false).unwrap();
        assert!(o.is_owned(owned_mp));
        assert!(!o.is_owned(preexisting_mp));
        assert_eq!(o.len(), 2);

        // A fresh load sees both entries.
        let reloaded = MountOwnership::load(path.clone());
        assert!(reloaded.is_owned(owned_mp));
        assert!(!reloaded.is_owned(preexisting_mp));

        // Reconcile prunes entries whose mount point is not live. Neither
        // simulated path is mounted in this environment, so both go.
        let mut reloaded = reloaded;
        reloaded.reconcile().unwrap();
        assert!(reloaded.is_empty(), "unmounted entries must be pruned");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownership_save_is_atomic_and_crash_safe() {
        // M18: a crash mid-write leaves the previous state intact because the
        // write goes to a temp file that is renamed.
        let dir =
            std::env::temp_dir().join(format!("omavrything-own-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mounts.state");

        let mut o = MountOwnership::load(path.clone());
        o.record(Path::new("/mp/a"), true).unwrap();
        let good = std::fs::read_to_string(&path).unwrap();

        // A leftover temp file from a crash must not be read as state.
        std::fs::write(path.with_extension("state.tmp"), b"garbage").unwrap();
        let reloaded = MountOwnership::load(path.clone());
        assert_eq!(reloaded.len(), 1);
        assert!(reloaded.is_owned(Path::new("/mp/a")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), good);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownership_forget_drops_the_entry() {
        let dir =
            std::env::temp_dir().join(format!("omavrything-own-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mounts.state");

        let mut o = MountOwnership::load(path.clone());
        o.record(Path::new("/mp/a"), true).unwrap();
        o.forget(Path::new("/mp/a")).unwrap();
        assert!(o.is_empty());
        assert!(MountOwnership::load(path).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_mount_entries_removes_a_vanished_mounts_subtree() {
        // M18 (B8): when a mount disappears, its index entries are dropped;
        // when it is still live, nothing is touched.
        let mut index = Index::new();
        let mp = Path::new("/run/omavrything/ntfs/vol");
        index.add_root(mp, FsKind::Ntfs);
        index.insert(&mp.join("hello.txt"), false, 1, 0);
        assert!(index.find(&mp.join("hello.txt")).is_some());

        // The mount point is not live in /proc/mounts, so the entries go.
        let manager = IndexManager::new(index, Vec::new(), None);
        assert!(drop_mount_entries(&manager, mp));
        assert!(
            manager
                .with_index_mut(|idx| idx.find(&mp.join("hello.txt")))
                .is_none()
        );

        // A mount point that is still live (any real mount, e.g. /) is kept.
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/etc-hosts-placeholder"), false, 1, 0);
        let manager = IndexManager::new(index, Vec::new(), None);
        assert!(!drop_mount_entries(&manager, Path::new("/")));
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

    #[test]
    fn policy_save_is_atomic_and_crash_safe() {
        // B7: the policy is written to a temp file and renamed, so a crash
        // mid-write leaves the previous policy intact.
        let dir =
            std::env::temp_dir().join(format!("omavrything-ntfs-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ntfs.state");

        let mut policy = NtfsPolicy::load(path.clone());
        policy.remember("AAAA-BBBB").unwrap();
        let good = std::fs::read_to_string(&path).unwrap();

        // A leftover temp file from a crash must not be read as state.
        std::fs::write(path.with_extension("state.tmp"), b"garbage").unwrap();
        let reloaded = NtfsPolicy::load(path.clone());
        assert!(reloaded.is_known("AAAA-BBBB"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), good);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_mount_name_prefers_uuid_and_falls_back_to_device() {
        // B6: a UUID-less volume uses its device basename, never the constant
        // `volume`, so two such volumes cannot collide.
        assert_eq!(
            private_mount_name(Path::new("/dev/sda1"), Some("AAAA-BBBB")),
            "AAAA-BBBB"
        );
        assert_eq!(private_mount_name(Path::new("/dev/sda1"), None), "sda1");
        assert_eq!(
            private_mount_name(Path::new("/dev/nvme0n1p2"), Some("")),
            "nvme0n1p2"
        );
        assert_eq!(
            private_mount_name(Path::new("/dev/disk/by-uuid/1234"), None),
            "1234"
        );
    }

    #[test]
    fn mount_options_are_read_only_and_hardened() {
        // B5: the daemon never mounts writable, setuid, device, or executable.
        let options: Vec<&str> = MOUNT_OPTIONS.split(',').collect();
        for required in ["ro", "nosuid", "nodev", "noexec"] {
            assert!(
                options.contains(&required),
                "missing mount option {required}"
            );
        }
    }

    #[test]
    fn command_timeout_kills_a_hung_process() {
        // B5: a hung mount/udisksctl call is killed instead of wedging the
        // startup or hotplug thread.
        let start = Instant::now();
        let err = run_command_timeout("sleep", &["5"], Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn two_uuid_less_volumes_get_distinct_private_mount_points() {
        // B6: two UUID-less volumes must not share the constant `volume`.
        let a = private_mount_name(Path::new("/dev/sda1"), None);
        let b = private_mount_name(Path::new("/dev/sdb1"), None);
        assert_ne!(a, b);
        assert_eq!(a, "sda1");
        assert_eq!(b, "sdb1");
    }

    #[test]
    fn private_mounts_from_filters_the_private_root() {
        // B4/M19: only mounts under /run/omavrything/ntfs are daemon-created.
        let text = "\
/dev/sda1 /run/omavrything/ntfs/sda1 ntfs3 ro,nosuid,nodev,noexec 0 0
/dev/sdb1 /run/media/sam/Data ntfs3 rw 0 0
/dev/sdc1 /run/omavrything/ntfs/AAAA-BBBB ntfs3 ro 0 0
";
        assert_eq!(
            private_mounts_from(text),
            vec![
                PathBuf::from("/run/omavrything/ntfs/sda1"),
                PathBuf::from("/run/omavrything/ntfs/AAAA-BBBB"),
            ]
        );
    }

    #[test]
    fn mount_is_readonly_from_reads_the_options_field() {
        // B5/B12: the read-only check is based on the mount options, not the
        // filesystem type or the source device.
        let text = "\
/dev/sda1 /run/omavrything/ntfs/sda1 ntfs3 ro,nosuid,nodev,noexec 0 0
/dev/sdb1 /run/media/sam/Data ntfs3 rw,relatime 0 0
";
        assert_eq!(
            mount_is_readonly_from(text, Path::new("/run/omavrything/ntfs/sda1")),
            Some(true)
        );
        assert_eq!(
            mount_is_readonly_from(text, Path::new("/run/media/sam/Data")),
            Some(false)
        );
        assert_eq!(
            mount_is_readonly_from(text, Path::new("/not/mounted")),
            None
        );
    }

    #[test]
    fn daemon_owned_mounts_never_selects_a_preexisting_mount() {
        // B1: a pre-existing udisks2 mount under /run/media is never selected
        // for unmount, even though it is live.
        let ownership = MountOwnership::default();
        let mounts = vec![
            PathBuf::from("/run/omavrything/ntfs/sda1"),
            PathBuf::from("/run/media/sam/Data"),
        ];
        assert_eq!(
            daemon_owned_mounts(mounts, &ownership),
            vec![PathBuf::from("/run/omavrything/ntfs/sda1")]
        );
    }
}

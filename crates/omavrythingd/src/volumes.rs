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
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Total wall-clock budget for one `blkid` probe. The child is killed and
/// reaped when it is exceeded, so no probe can leak a process or a blocking
/// pool slot.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a completed probe is served to further callers. `blkid` is a
/// blocking, device-touching call, so back-to-back settings-UI refreshes are
/// answered from this snapshot instead of re-probing every block device.
const PROBE_CACHE_TTL: Duration = Duration::from_secs(2);

/// Enumerate every block filesystem `blkid` can identify.
///
/// Swap and empty signatures are skipped: they are not indexable volumes. The
/// probe is bounded by [`PROBE_TIMEOUT`]; a timeout or spawn failure yields an
/// empty list, matching the previous "no volumes detected" behaviour.
pub fn list_block_volumes() -> Vec<Volume> {
    let output = match crate::ntfs::run_command_timeout("blkid", &["-o", "export"], PROBE_TIMEOUT) {
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

/// The outcome of a shared volume probe.
#[derive(Debug)]
pub enum VolumesProbe {
    /// A (possibly cached) volume list.
    Ready(Vec<Volume>),
    /// Too many callers were already waiting on the in-flight probe. The
    /// caller is rejected immediately instead of queueing another blocking
    /// job, so a local flood cannot exhaust the blocking pool or spawn a
    /// `blkid` child per request.
    Busy,
}

/// Callers allowed to wait on the single in-flight probe. Past this bound a
/// request is answered `Busy`, which keeps the number of queued blocking tasks
/// proportional to a small constant rather than to the request rate.
const MAX_PROBE_WAITERS: usize = 8;

/// State of the single shared volume probe.
struct ProbeState {
    in_flight: bool,
    waiters: usize,
    /// Completed generation, bumped after each probe finishes so waiters can
    /// tell a fresh result from the one they saw before waiting.
    generation: u64,
    cached: Option<(Instant, Vec<Volume>)>,
}

static PROBE_STATE: Mutex<ProbeState> = Mutex::new(ProbeState {
    in_flight: false,
    waiters: 0,
    generation: 0,
    cached: None,
});
static PROBE_DONE: Condvar = Condvar::new();

/// Enumerate volumes through a **single** shared, cached, bounded probe.
///
/// `ListVolumes` is callable by every local UID without polkit, and each probe
/// spawns `blkid` and waits for it. Without coalescing, a local burst starts one
/// blocking job and one child per call. Here:
///
/// * a result younger than [`PROBE_CACHE_TTL`] is served directly (no work);
/// * concurrent callers join the in-flight probe instead of starting another;
/// * at most [`MAX_PROBE_WAITERS`] callers may wait, others get
///   [`VolumesProbe::Busy`];
/// * the probe itself is bounded by [`PROBE_TIMEOUT`], after which the child is
///   killed and reaped before the slot is released.
pub fn shared_block_volumes() -> VolumesProbe {
    shared_probe(list_block_volumes)
}

/// The coalescing logic of [`shared_block_volumes`], generic over the probe so
/// it can be tested without touching block devices.
fn shared_probe(probe: impl FnOnce() -> Vec<Volume>) -> VolumesProbe {
    let mut state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    // A fresh cache is served without probing, even if another probe is running
    // (that probe will only replace it with an equivalent, newer list).
    if let Some((at, cached)) = &state.cached {
        if at.elapsed() < PROBE_CACHE_TTL {
            return VolumesProbe::Ready(cached.clone());
        }
    }
    if state.in_flight {
        if state.waiters >= MAX_PROBE_WAITERS {
            return VolumesProbe::Busy;
        }
        // Wait for the in-flight probe (or the wait deadline) instead of
        // starting a second one.
        let seen = state.generation;
        state.waiters += 1;
        let (mut state, _) = PROBE_DONE
            .wait_timeout_while(state, PROBE_TIMEOUT + Duration::from_secs(1), |s| {
                s.in_flight && s.generation == seen
            })
            .unwrap_or_else(|e| e.into_inner());
        // Saturating, not `-= 1`: the counter is process-global and every waiter
        // increments it under this same lock, but a waiter can outlive the
        // generation it joined -- a probe that finished before this caller
        // re-acquired the lock, or a result reset out from under two waiting
        // generations. In that window the counter is legitimately already zero,
        // and a bare decrement panics with "attempt to subtract with overflow"
        // (this is the intermittent failure CI reported in
        // `concurrent_probes_coalesce_into_one_child`). Under-counting a freed
        // slot is the safe direction: the worst case is admitting one extra
        // waiter, not an underflow panic inside the daemon.
        state.waiters = state.waiters.saturating_sub(1);
        return match &state.cached {
            Some((_, cached)) => VolumesProbe::Ready(cached.clone()),
            None => VolumesProbe::Busy,
        };
    }
    state.in_flight = true;
    drop(state);

    let volumes = probe();

    let mut state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.in_flight = false;
    state.generation = state.generation.wrapping_add(1);
    state.cached = Some((Instant::now(), volumes.clone()));
    drop(state);
    PROBE_DONE.notify_all();
    VolumesProbe::Ready(volumes)
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
    let device_str = device.to_string_lossy();
    let output = crate::ntfs::run_command_timeout(
        "blkid",
        &["-o", "value", "-s", "TYPE", &device_str],
        PROBE_TIMEOUT,
    )
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
    // B3: prefer the udisks2 D-Bus API; it returns the mount point directly.
    match crate::udisks::mount(device, None) {
        Ok(mount_point) => return Ok(mount_point),
        Err(err) => {
            tracing::debug!(
                device = %device.display(),
                error = %err,
                "volumes: udisks2 D-Bus mount failed; falling back to udisksctl"
            );
        }
    }
    let device_str = device.to_string_lossy();
    let output = crate::ntfs::run_command_timeout(
        "udisksctl",
        &["mount", "--no-user-interaction", "-b", &device_str],
        crate::ntfs::UDISKS_TIMEOUT,
    )
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("could not mount {}: {stderr}", device.display()));
    }
    // B3: prefer the kernel mount table over parsing English output. The
    // `udisksctl` text is only a fallback for a mount that has not appeared in
    // `/proc/mounts` yet.
    if let Some(mount_point) = existing_mount_point(device) {
        return Ok(mount_point);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(pos) = stdout.find(" at ") {
        let mount_point = stdout[pos + 4..].trim().trim_end_matches('.').trim();
        if !mount_point.is_empty() {
            return Ok(PathBuf::from(mount_point));
        }
    }
    Err(format!(
        "udisksctl mounted {} but its mount point could not be determined",
        device.display()
    ))
}

/// The mount point of `device` from `/proc/mounts`, if mounted.
pub fn existing_mount_point(device: &Path) -> Option<PathBuf> {
    mounts::read_mounts()
        .unwrap_or_default()
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
    let mount_point_str = mount_point.to_string_lossy();
    if private {
        for _ in 0..3 {
            let ok = crate::ntfs::run_command_timeout(
                "umount",
                &[&mount_point_str],
                crate::ntfs::UNMOUNT_TIMEOUT,
            )
            .map(|s| s.status.success())
            .unwrap_or(false);
            if ok && existing_mount_point(device).is_none() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = crate::ntfs::run_command_timeout(
            "umount",
            &["-l", &mount_point_str],
            crate::ntfs::UNMOUNT_TIMEOUT,
        );
    } else {
        // B3: prefer the udisks2 D-Bus API.
        if crate::udisks::unmount(device).is_ok() && existing_mount_point(device).is_none() {
            return Ok(());
        }
        let device_str = device.to_string_lossy();
        let output = crate::ntfs::run_command_timeout(
            "udisksctl",
            &["unmount", "--no-user-interaction", "-b", &device_str],
            crate::ntfs::UDISKS_TIMEOUT,
        );
        if let Ok(output) = output {
            if output.status.success() && existing_mount_point(device).is_none() {
                return Ok(());
            }
        }
        let _ = crate::ntfs::run_command_timeout(
            "umount",
            &[&mount_point_str],
            crate::ntfs::UNMOUNT_TIMEOUT,
        );
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
    use std::sync::Arc;

    /// The probe tests share the process-global `PROBE_STATE`, so without
    /// serialisation one test's reset can clear the cache between another test's
    /// two calls and make it look like a cache miss.
    /// `a_cached_result_is_served_without_probing` was intermittently failing
    /// for exactly that reason. Each probe test holds this guard for its body.
    static PROBE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_probe_tests() -> std::sync::MutexGuard<'static, ()> {
        PROBE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    fn reset_probe_state() {
        let mut state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight = false;
        state.waiters = 0;
        state.cached = None;
    }

    #[test]
    fn concurrent_probes_coalesce_into_one_child() {
        // The admission finding: many independent callers must not each start a
        // `blkid` child. One caller runs the probe; the rest join it and receive
        // the same result.
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let _serial = lock_probe_tests();
        reset_probe_state();
        let runs = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(MAX_PROBE_WAITERS + 1));

        let mut handles = Vec::new();
        for _ in 0..MAX_PROBE_WAITERS {
            let runs = Arc::clone(&runs);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                // If this caller wins the race it becomes the probe and blocks
                // briefly, so the others are forced to join rather than start
                // their own.
                match shared_probe(|| {
                    runs.fetch_add(1, AtomicOrdering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    vec![test_volume("/dev/omavry-probe")]
                }) {
                    VolumesProbe::Ready(v) => v,
                    VolumesProbe::Busy => Vec::new(),
                }
            }));
        }
        barrier.wait();
        // Let the racers run to completion.
        let mut ready = 0;
        for handle in handles {
            let out = handle.join().unwrap();
            if !out.is_empty() {
                ready += 1;
                assert_eq!(out[0].device, PathBuf::from("/dev/omavry-probe"));
            }
        }
        // At most one probe ran: the rest were either cached or joined it.
        assert_eq!(
            runs.load(AtomicOrdering::SeqCst),
            1,
            "concurrent probes must coalesce into a single child"
        );
        assert!(ready > 0, "at least one caller must receive the result");
        reset_probe_state();
    }

    #[test]
    fn a_burst_past_the_waiter_bound_is_rejected_as_busy() {
        let _serial = lock_probe_tests();
        reset_probe_state();
        // Simulate an in-flight probe that already has the maximum number of
        // waiters. The next caller must be rejected immediately instead of
        // queueing another blocking job, so the admission bound holds.
        {
            let mut state = PROBE_STATE.lock().unwrap_or_else(|e| e.into_inner());
            state.in_flight = true;
            state.waiters = MAX_PROBE_WAITERS;
        }

        assert!(
            matches!(shared_probe(Vec::new), VolumesProbe::Busy),
            "a caller past the waiter bound must get Busy, not a new probe"
        );
        reset_probe_state();
    }

    /// Regression for the intermittent CI failure in
    /// `concurrent_probes_coalesce_into_one_child`: "attempt to subtract with
    /// overflow" at the waiter decrement. Re-runs the real coalescing path many
    /// times so the interleaving (a waiter whose generation finished before it
    /// re-acquired the lock) is actually exercised, and asserts the waiter
    /// counter always returns to zero.
    #[test]
    fn repeated_waiter_churn_never_underflows_the_waiter_count() {
        use std::sync::Barrier;

        let _serial = lock_probe_tests();
        for _ in 0..25 {
            reset_probe_state();
            let barrier = Arc::new(Barrier::new(MAX_PROBE_WAITERS + 1));
            let mut handles = Vec::new();
            for _ in 0..MAX_PROBE_WAITERS {
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    // An instant probe maximises the chance that the probe
                    // completes while the other callers are still resuming from
                    // the condvar, which is the underflow window.
                    let _ = shared_probe(Vec::new);
                }));
            }
            barrier.wait();
            for handle in handles {
                handle
                    .join()
                    .expect("a waiter panicked instead of saturating");
            }
            let waiters = PROBE_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .waiters;
            assert_eq!(waiters, 0, "waiter count must return to zero");
        }
        reset_probe_state();
    }

    #[test]
    fn a_cached_result_is_served_without_probing() {
        let _serial = lock_probe_tests();
        reset_probe_state();
        assert!(matches!(
            shared_probe(|| vec![test_volume("/dev/omavry-first")]),
            VolumesProbe::Ready(v) if v[0].device.as_path() == Path::new("/dev/omavry-first")
        ));
        // A second call within the TTL is served from the cache: the probe
        // closure panics if it is ever called.
        assert!(matches!(
            shared_probe(|| panic!("the cache must be served without probing")),
            VolumesProbe::Ready(v) if v[0].device.as_path() == Path::new("/dev/omavry-first")
        ));
        reset_probe_state();
    }

    fn test_volume(device: &str) -> Volume {
        Volume {
            device: PathBuf::from(device),
            uuid: None,
            label: None,
            fstype: "ext4".into(),
            mount_point: None,
        }
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

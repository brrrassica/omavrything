//! The `omavrythingd` index daemon.
//!
//! Discovers mounted filesystems (or uses explicit roots), scans them in
//! parallel, keeps the index current with inotify, runs idle-gated periodic
//! rescans, and persists a snapshot for warm starts. Queries are served as
//! newline-delimited JSON over a Unix socket; control and status are exposed on
//! D-Bus.

mod config;
mod config_file;
mod dbus;
mod logging;
mod manager;
mod ntfs;
mod peer;
mod polkit;

use std::ffi::CString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omavrything_core::VERSION;
use omavrything_core::idle::{IdleMonitor, IdleThresholds, RescanScheduler};
use omavrything_core::index::Index;
use omavrything_core::live::FsEvent;
use omavrything_core::protocol::{IndexState, Method, Response, ResultItem, parse_request};
use omavrything_core::scan::{ScanConfig, apply_rescan, plan_rescan, scan};
use omavrything_core::snapshot::{self, WarmStart};
use omavrything_core::watch::Watcher;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use config::{ConfigInit, SharedConfig};
use config_file::{DEFAULT_CONFIG_PATH, FileConfig};
use dbus::IndexEvent;
use logging::LogLevel;
use manager::IndexManager;

/// Maximum number of concurrent client connections.
const MAX_CONNECTIONS: usize = 64;
/// Per-connection read timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the watcher blocks waiting for events before checking pending moves.
const WATCH_POLL: Duration = Duration::from_millis(500);
/// Default interval between periodic snapshot writes.
const DEFAULT_SNAPSHOT_INTERVAL: u64 = 300;
/// Idle period (seconds) after which the index is evicted to its snapshot image.
/// The settings menu offers 10-60 s in 10 s steps; 20 s is the default.
const EVICT_IDLE_SECS: u64 = 20;
/// The group that may use the system socket.
const SOCKET_GROUP: &str = "omavrything";

/// Serializes snapshot writes between the periodic writer and the shutdown
/// handler, which share a fixed temp path.
static SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());

fn main() {
    // Constrain the allocator before any thread spawns so the scan's transient
    // buffers are returned to the OS rather than retained in per-thread arenas.
    tune_allocator();
    // Bound the rayon pool before the scan builds it, so the daemon does not
    // spawn one worker per core for an I/O-bound walk (rc6 RSS spike).
    tune_scan_pool();
    let args = match Args::parse() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("omavrythingd: {err}");
            std::process::exit(2);
        }
    };
    logging::init(args.log_level);

    info!(version = VERSION, "omavrythingd starting");
    let scan_config = args.scan_config();
    let started = Instant::now();

    let (mut index, report, identities, outcome) = match &args.snapshot {
        Some(path) => {
            // Load the snapshot through an mmap (F6): it avoids the intermediate
            // read buffer, and falls back to a plain read if mapping fails.
            let warm = snapshot::warm_start_with(&scan_config, path, snapshot::SnapshotLoad::Mmap);
            (warm.index, warm.report, warm.identities, Some(warm.outcome))
        }
        None => {
            let (index, report) = scan(&scan_config);
            let identities = index
                .roots()
                .iter()
                .filter_map(|r| snapshot::root_identity(Path::new(&r.path)))
                .collect();
            (index, report, identities, None)
        }
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Some(WarmStart::Snapshot) => info!(
            entries = %index.len(),
            elapsed_ms,
            "warm-started from snapshot"
        ),
        Some(WarmStart::Partial { rescanned }) => info!(
            entries = %index.len(),
            rescanned,
            elapsed_ms,
            "warm-started from snapshot"
        ),
        Some(WarmStart::ColdScan) => info!(
            entries = %report.entries,
            roots = %report.roots,
            skipped = %report.skipped,
            unreadable = %report.errors,
            elapsed_ms,
            "no usable snapshot; cold scan"
        ),
        None => info!(
            entries = %report.entries,
            roots = %report.roots,
            skipped = %report.skipped,
            unreadable = %report.errors,
            elapsed_ms,
            "indexed"
        ),
    }
    for root in index.roots() {
        info!(path = %root.path, fs_kind = ?root.fs_kind, "root");
    }

    // F10: honour the candidate-index preference before the index settles, so a
    // disabled index is not consulted (and its memory is reclaimed).
    if !args.candidate_index {
        index.set_candidate_enabled(false);
    }
    // F7: the benchmark disables the query cache to measure the cold path.
    if !args.query_cache {
        index.set_query_cache(false);
    }

    // Reclaim the doubling slack left by the scan or snapshot load, then return
    // the scan's freed transient pages to the OS before the daemon idles.
    index.shrink();
    trim_heap();
    log_memory(&index, "startup");

    let index = Arc::new(IndexManager::new(index, identities, args.snapshot.clone()));
    let config = Arc::new(SharedConfig::new(ConfigInit {
        scan: scan_config,
        scan_interval: args.scan_interval,
        snapshot_interval: args.snapshot_interval,
        evict_idle_secs: args.evict_idle_secs,
        snapshot: args.snapshot.clone(),
        watch: args.watch,
        ntfs: args.ntfs,
        log_level: args.log_level,
        background_opacity: args.background_opacity,
        candidate_index: args.candidate_index,
        state: args.state.clone(),
        state_path: args.state_path.clone(),
    }));
    let policy = Arc::new(Mutex::new(ntfs::NtfsPolicy::load(
        ntfs::default_policy_path(),
    )));

    // Index events are forwarded to the D-Bus signal task. If D-Bus is
    // disabled the receiver is dropped and sends are ignored.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<IndexEvent>();

    // Block SIGTERM/SIGINT before spawning threads so they inherit the mask and
    // a dedicated thread can perform a clean snapshot write on shutdown.
    install_shutdown_handler(Arc::clone(&index), args.snapshot.clone());

    if args.watch {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        spawn_thread("omavry-watch", CONTROL_STACK, move || {
            run_watcher(index, config, events)
        });
    }

    if args.scan_interval > 0 {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        spawn_thread("omavry-sched", CONTROL_STACK, move || {
            run_scheduler(index, config, events)
        });
    }

    if let Some(path) = &args.snapshot {
        if args.snapshot_interval > 0 {
            let index = Arc::clone(&index);
            let config = Arc::clone(&config);
            let path = path.clone();
            spawn_thread("omavry-snap", CONTROL_STACK, move || {
                run_snapshot(index, config, path)
            });
        }
        // Eviction needs a snapshot to map, so only run the evictor when one is
        // configured.
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        spawn_thread("omavry-evict", CONTROL_STACK, move || {
            run_evictor(index, config)
        });
    }

    if args.dbus {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        let policy = Arc::clone(&policy);
        spawn_thread("omavry-dbus", CONTROL_STACK, move || {
            dbus::run(index, config, started, event_rx, events, policy)
        });
    }

    if args.ntfs {
        let index = Arc::clone(&index);
        let policy = Arc::clone(&policy);
        spawn_thread("omavry-ntfs", CONTROL_STACK, move || {
            run_ntfs_startup(index, policy)
        });
    }

    // A stale socket from a previous run would make bind() fail.
    if let Some(parent) = args.socket.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            warn!(path = %parent.display(), error = %err, "failed to create socket directory");
        }
    }
    // Refuse to start a second daemon on the same socket.
    let _singleton = match acquire_singleton(&args.socket) {
        Ok(file) => file,
        Err(err) => {
            error!(
                socket = %args.socket.display(),
                error = %err,
                "another omavrythingd appears to be running"
            );
            std::process::exit(1);
        }
    };
    let _ = std::fs::remove_file(&args.socket);
    let listener = match UnixListener::bind(&args.socket) {
        Ok(listener) => listener,
        Err(err) => {
            error!(socket = %args.socket.display(), error = %err, "failed to bind socket");
            std::process::exit(1);
        }
    };
    if let Err(err) = prepare_socket(&args.socket) {
        warn!(socket = %args.socket.display(), error = %err, "could not secure socket");
    }
    info!(socket = %args.socket.display(), "listening");

    let allowed_gid = peer::group_gid(SOCKET_GROUP);
    if allowed_gid.is_none() {
        warn!(
            group = SOCKET_GROUP,
            "group not found; relying on socket permissions"
        );
    }

    // Opt-in per-query profiling: when `OMAVRYTHING_PROFILE` is set, every
    // search logs a stage-by-stage timing breakdown to stderr. Off by default so
    // the hot path pays nothing.
    let profile = std::env::var_os("OMAVRYTHING_PROFILE").is_some();
    if profile {
        info!("query profiling enabled (OMAVRYTHING_PROFILE set)");
    }

    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                match peer::peer_cred(&stream) {
                    Some(cred) if peer::peer_allowed(&cred, allowed_gid) => {}
                    Some(cred) => {
                        warn!(
                            uid = cred.uid,
                            gid = cred.gid,
                            pid = cred.pid,
                            "rejecting connection"
                        );
                        drop(stream);
                        continue;
                    }
                    None => {
                        warn!("rejecting connection: could not read peer credentials");
                        drop(stream);
                        continue;
                    }
                }
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    warn!("connection limit reached; dropping client");
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let index = Arc::clone(&index);
                let active = Arc::clone(&active);
                spawn_thread("omavry-conn", CONNECTION_STACK, move || {
                    serve(stream, index, profile);
                    active.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(err) => warn!(error = %err, "accept error"),
        }
    }
}

/// Set the socket to mode `0660` and, when running as root, group
/// `omavrything`.
fn prepare_socket(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        // Prefer the `omavrything` group. When it does not exist (a dev setup),
        // hand the socket to the user who invoked `sudo` so their session can
        // connect; otherwise the socket would be root-only and the shell plugin
        // could not search.
        let (uid, gid) = match peer::group_gid(SOCKET_GROUP) {
            Some(gid) => (!0 as libc::uid_t, gid),
            None => match sudo_invoker() {
                Some(owner) => owner,
                None => return Ok(()),
            },
        };
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path has NUL"))?;
        // SAFETY: `c` is a valid NUL-terminated path.
        let rc = unsafe { libc::chown(c.as_ptr(), uid, gid) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The uid/gid of the user who invoked `sudo`, from `SUDO_UID`/`SUDO_GID`.
fn sudo_invoker() -> Option<(libc::uid_t, libc::gid_t)> {
    let uid = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    Some((uid, gid))
}

/// Acquire an exclusive lock on `<socket>.lock` so only one daemon runs per
/// socket. The returned `File` must be kept alive for the process lifetime.
fn acquire_singleton(socket: &Path) -> io::Result<File> {
    let lock_path = with_lock_suffix(socket);
    let file = File::create(&lock_path)?;
    // SAFETY: `file` is a valid open fd; `flock` has no other preconditions.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// The lock-file path for a socket path.
fn with_lock_suffix(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Query the systemd login1 idle hint. Returns `None` when login1 is
/// unavailable, in which case the caller must not treat it as a veto.
fn login1_idle_hint(rt: &tokio::runtime::Runtime) -> Option<bool> {
    rt.block_on(async {
        let connection = zbus::Connection::system().await.ok()?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        .ok()?;
        proxy.get_property::<bool>("IdleHint").await.ok()
    })
}

/// Watch the indexed roots with inotify and apply events to the index.
fn run_watcher(
    index: Arc<IndexManager>,
    config: Arc<SharedConfig>,
    events: UnboundedSender<IndexEvent>,
) {
    let mut watcher = match Watcher::new() {
        Ok(w) => w,
        Err(err) => {
            error!(error = %err, "inotify init failed");
            return;
        }
    };

    let roots: Vec<PathBuf> = index.roots().iter().map(|r| PathBuf::from(&r.path)).collect();
    for root in &roots {
        if let Err(err) = watcher.watch_dir(root) {
            warn!(path = %root.display(), error = %err, "watch failed");
        }
    }
    let mut reported_failed = watcher.failed().len();
    if reported_failed > 0 {
        warn!(
            failed = reported_failed,
            "directories could not be watched (max_user_watches exhausted?)"
        );
    }
    info!(directories = watcher.watch_count(), "watching");

    let mut buffer = [0u8; 8192];
    loop {
        match watcher.read_events(&mut buffer, WATCH_POLL) {
            Ok(batch) => {
                if batch.is_empty() {
                    continue;
                }
                let scan = config.scan();
                for event in &batch {
                    index.apply_event(event, &scan);
                }
                // Reclaim the doubling slack the inserts left, so a long-running
                // daemon does not hold 2x capacity forever (rc6 RSS spike). A
                // no-op when the index is evicted or the slack is small.
                if index.shrink_if_owned() {
                    // The shrink freed pages; return them to the OS rather than
                    // leaving them in the glibc arena (rc6 RSS spike).
                    trim_heap();
                }
                // Watch newly created directories.
                for event in &batch {
                    if let FsEvent::Created(path) = event {
                        if path.is_dir() {
                            let _ = watcher.watch_dir(path);
                        }
                    }
                }
                let failed = watcher.failed().len();
                if failed > reported_failed {
                    warn!(
                        additional = failed - reported_failed,
                        "more directories could not be watched (max_user_watches exhausted?)"
                    );
                    reported_failed = failed;
                }
                let entries = index.len() as u64;
                let _ = events.send(IndexEvent::Changed { entries });
            }
            Err(err) => {
                error!(error = %err, "inotify read error");
                break;
            }
        }
    }
}

/// Periodically rescan the roots when the system is idle.
fn run_scheduler(
    index: Arc<IndexManager>,
    config: Arc<SharedConfig>,
    events: UnboundedSender<IndexEvent>,
) {
    let mut monitor = IdleMonitor::new(IdleThresholds::default());
    let mut scheduler = RescanScheduler::new(Duration::from_secs(config.scan_interval().max(1)));
    // A current-thread runtime for the login1 idle-hint query. If it cannot be
    // built, the hint is simply unavailable.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok();
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let interval = config.scan_interval();
        if interval == 0 {
            continue;
        }
        scheduler.set_interval(Duration::from_secs(interval));
        // The login1 hint is a secondary signal: when unavailable it does not
        // veto a rescan.
        let login1_idle = rt.as_ref().and_then(login1_idle_hint).unwrap_or(true);
        let idle = monitor.is_idle() && login1_idle;
        if scheduler.should_rescan(Instant::now(), idle) {
            info!("idle: rescanning roots");
            let _ = events.send(IndexEvent::ScanProgress {
                phase: "start".into(),
                entries: 0,
            });
            config.set_scanning(true);
            let scan = config.scan();
            // Walk the filesystem outside the lock, then reconcile under it.
            // This keeps searches unblocked during the traversal without cloning
            // the whole index (which doubled peak memory).
            let plan = plan_rescan(&scan);
            let (report, entries) = index.with_index_mut(|idx| {
                let report = apply_rescan(idx, plan);
                // A rescan appends the new entries and leaves the vectors at
                // their doubled capacity; reclaim it (rc6 RSS spike).
                idx.shrink();
                (report, idx.len() as u64)
            });
            config.set_scanning(false);
            trim_heap();
            info!(
                added = %report.entries,
                stale = %report.skipped,
                unreadable = %report.errors,
                "rescan complete"
            );
            let _ = events.send(IndexEvent::ScanProgress {
                phase: "done".into(),
                entries,
            });
            let _ = events.send(IndexEvent::Changed { entries });
        }
    }
}

/// Detect NTFS partitions at startup: auto-mount and index previously indexed
/// ones, and log newly seen ones as offers.
/// Run `job`, converting a panic into `Err` so that one bad input cannot stop a
/// whole startup pass.
fn contain_panic<T>(job: impl FnOnce() -> T) -> Result<T, ()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).map_err(|_| ())
}

pub(crate) fn run_ntfs_startup(index: Arc<IndexManager>, policy: Arc<Mutex<ntfs::NtfsPolicy>>) {
    {
        let p = policy.lock().unwrap_or_else(|e| e.into_inner());
        if !p.is_empty() {
            info!(partitions = p.len(), "ntfs: previously indexed partitions");
        }
    }
    let partitions = omavrything_ntfs::list_ntfs_partitions();
    for part in &partitions {
        let uuid = part.uuid.clone().unwrap_or_default();
        let known = {
            let p = policy.lock().unwrap_or_else(|e| e.into_inner());
            !uuid.is_empty() && p.is_known(&uuid)
        };
        // A partition that is already mounted is indexed; an unmounted one is
        // auto-mounted only if it was previously indexed, otherwise offered.
        let mounted = ntfs::existing_mount_point(&part.device).is_some();
        if !known && !mounted {
            info!(
                device = %part.device.display(),
                label = part.label.as_deref().unwrap_or("no label"),
                "ntfs: new partition; use MountNtfs to index it"
            );
            continue;
        }
        // Parsing reads attacker-controlled on-disk structures. Contain a panic to
        // this one partition: without it, a single malformed volume unwinds the
        // whole startup thread and no later partition is indexed at all. The index
        // and policy locks are reached through `unwrap_or_else(|e| e.into_inner())`
        // everywhere, so a lock poisoned by a contained panic stays usable.
        match contain_panic(|| ntfs::mount_index_and_remember(&index, &policy, part)) {
            Ok(Ok(mount_point)) => info!(
                device = %part.device.display(),
                mount_point = %mount_point.display(),
                "ntfs: indexed"
            ),
            Ok(Err(err)) => error!(
                device = %part.device.display(),
                error = %err,
                "ntfs: failed to index"
            ),
            Err(()) => error!(
                device = %part.device.display(),
                "ntfs: panic while indexing; skipping this partition"
            ),
        }
    }
    // MFT parsing allocates large transient buffers and the re-index appends to
    // the arenas; compact, reclaim the doubling slack, and return the freed pages
    // before the daemon idles.
    index.with_index_mut(|idx| {
        idx.compact();
        idx.shrink();
        let b = idx.memory_breakdown();
        info!(
            records_len = b.records_len,
            records_cap = b.records_cap,
            paths_len = b.paths_len,
            paths_cap = b.paths_cap,
            "index shrunk after NTFS indexing"
        );
    });
    trim_heap();
}

/// Periodically write a snapshot of the index.
/// Evict the index to its snapshot image after a period with no queries or
/// mutations, so the daemon's resident set falls to the mapping plus the delta.
fn run_evictor(index: Arc<IndexManager>, config: Arc<SharedConfig>) {
    loop {
        let idle = config.evict_idle_secs();
        // Poll at most every 30 s, and at least as often as the idle period, so
        // a short configured period is honoured without a busy loop.
        let poll = if idle == 0 { 30 } else { idle.clamp(1, 30) };
        std::thread::sleep(Duration::from_secs(poll));
        if index.evict_if_idle(idle) {
            info!(idle_secs = idle, "index evicted (idle)");
            trim_heap();
        }
    }
}

fn run_snapshot(index: Arc<IndexManager>, config: Arc<SharedConfig>, path: PathBuf) {
    loop {
        let interval = config.snapshot_interval();
        if interval == 0 {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        std::thread::sleep(Duration::from_secs(interval));
        // When evicted with no pending delta, the on-disk snapshot is already
        // current (eviction wrote it), so there is nothing to persist.
        if index.is_evicted() && !index.has_pending_delta() {
            continue;
        }
        // Compact and encode under the lock, then write outside it so the
        // inotify writer is not blocked on disk I/O.
        let ids = index.identities();
        let bytes = index.with_index_mut(|idx| {
            idx.compact();
            snapshot::encode(idx, &ids)
        });
        let _guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = snapshot::write_atomic(&bytes, &path) {
            error!(error = %e, "snapshot save failed");
        }
        drop(bytes);
        // The encoded snapshot is a large transient buffer; return its pages.
        trim_heap();
    }
}

/// Write end of the self-pipe used by the signal handler, or -1.
static SHUTDOWN_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe handler: wake the shutdown thread via the self-pipe.
extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    let fd = SHUTDOWN_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // SAFETY: `write` is async-signal-safe and `byte` is a valid 1-byte
        // buffer. The return value is intentionally ignored.
        unsafe {
            libc::write(fd, byte.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Install a SIGTERM/SIGINT handler that writes a snapshot and exits cleanly.
///
/// A self-pipe is used: the signal handler (which must be async-signal-safe)
/// only writes a byte, and a dedicated thread performs the snapshot write. The
/// handler is installed process-wide; it does not block the signals.
fn install_shutdown_handler(index: Arc<IndexManager>, snapshot: Option<PathBuf>) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        error!(
            error = %io::Error::last_os_error(),
            "failed to create shutdown pipe"
        );
        return;
    }
    SHUTDOWN_PIPE.store(fds[1], Ordering::Relaxed);
    let handler: extern "C" fn(libc::c_int) = handle_shutdown_signal;
    // SAFETY: `handle_shutdown_signal` is a valid signal handler.
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }

    let read_fd = fds[0];
    spawn_thread("omavry-shutdown", CONTROL_STACK, move || {
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: `read_fd` is a valid pipe read end and `byte` is 1 byte.
            let n = unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
            if n == 1 {
                break;
            }
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
        }
        info!("shutdown signal received");
        if let Some(path) = &snapshot {
            let bytes = index.encode();
            let _guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            match snapshot::write_atomic(&bytes, path) {
                Ok(()) => info!(path = %path.display(), "snapshot written"),
                Err(e) => error!(error = %e, "snapshot save failed"),
            }
        }
        // Diagnostics: the daemon does not unmount the private NTFS mounts it
        // created (a known defect, see plans/risks.md R13). Log what leaks so
        // the mount-lifecycle question can be answered from logs.
        let leaked = ntfs::private_mounts();
        if !leaked.is_empty() {
            warn!(
                mounts = ?leaked,
                "ntfs: daemon-created mounts are NOT unmounted on shutdown (known defect; plans/risks.md R13)"
            );
        }
        std::process::exit(0);
    });
}

/// Serve one client connection: one request per line, one response per line.
///
/// When `profile` is set, each search logs a stage-by-stage timing breakdown to
/// stderr (see `OMAVRYTHING_PROFILE`).
fn serve(stream: UnixStream, index: Arc<IndexManager>, profile: bool) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let read_half = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let mut writer = stream;
    let mut reader = BufReader::new(read_half);

    loop {
        let line = match read_line_bounded(
            &mut reader,
            omavrything_core::protocol::MAX_LINE_BYTES,
            READ_TIMEOUT,
        ) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request = match parse_request(line.as_bytes()) {
            Ok(request) => request,
            Err(err) => {
                let _ = write_response(&mut writer, &Response::error(err));
                continue;
            }
        };

        let response = match request.method {
            Method::Search => {
                let started = Instant::now();
                let limit = request.effective_limit();
                let offset = request.effective_offset();
                let (outcome, loading) = index.search(&request.q, limit, offset);
                if profile {
                    debug!(query = %request.q, hits = outcome.hits.len(), "search");
                }
                let results: Vec<ResultItem> = outcome
                    .hits
                    .iter()
                    .map(|hit| ResultItem {
                        name: hit.name.clone(),
                        path: hit.path.clone(),
                        size: hit.size,
                        mtime: hit.mtime,
                        is_dir: hit.is_dir,
                    })
                    .collect();
                Response {
                    results,
                    total: outcome.total,
                    took_ms: started.elapsed().as_millis() as u64,
                    error: None,
                    state: if loading {
                        Some(IndexState::Loading)
                    } else {
                        None
                    },
                }
            }
        };

        if write_response(&mut writer, &response).is_err() {
            break;
        }
    }
}

/// Serialize and write one response line.
fn write_response(writer: &mut UnixStream, response: &Response) -> io::Result<()> {
    let json = serde_json::to_string(response).map_err(io::Error::other)?;
    writeln!(writer, "{json}")
}

/// Read one newline-terminated line, up to `max` bytes. Returns `Ok(None)` at
/// EOF. A line longer than `max`, or a request that takes longer than
/// `deadline` in total, is an error, so the connection is dropped rather than
/// buffering unbounded input or letting a slow client hold a slot.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    max: usize,
    deadline: Duration,
) -> io::Result<Option<String>> {
    let start = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if start.elapsed() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request deadline exceeded",
            ));
        }
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if buf.len() >= max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Log the index's used and resident (capacity) memory, so the benchmark can
/// attribute the gap between the accounting and RSS (rc7).
fn log_memory(index: &Index, phase: &str) {
    let b = index.memory_breakdown();
    info!(
        phase,
        entries = index.len(),
        memory_bytes = index.memory_bytes(),
        capacity_bytes = index.capacity_bytes(),
        records_len = b.records_len,
        records_cap = b.records_cap,
        paths_len = b.paths_len,
        paths_cap = b.paths_cap,
        map_len = b.map_len,
        map_cap_bytes = b.map_cap_bytes,
        candidate_bytes = b.candidate_bytes,
        candidate_capacity_bytes = b.candidate_capacity_bytes,
        "index memory"
    );
}

/// Ask the allocator to return freed heap pages to the OS.
///
/// A rescan allocates large transient buffers (the walked entry list) and, on
/// glibc, the freed pages are otherwise retained in the process's arenas, so RSS
/// stays at the peak long after the buffers are dropped. This is a best-effort
/// hint and a no-op on non-glibc targets.
fn trim_heap() {
    #[cfg(target_env = "gnu")]
    // SAFETY: `malloc_trim` has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Constrain the glibc allocator so freed scan buffers are returned to the OS.
///
/// By default glibc creates one arena per thread, and `malloc_trim` only
/// reliably releases the main arena, so the scan's transient allocations (made
/// on rayon worker threads) can stay resident. Forcing a single arena makes
/// [`trim_heap`] effective. A no-op on non-glibc targets.
fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    // SAFETY: `mallopt` has no preconditions; the options are valid.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
        // Make large transient buffers (the scan's walk buffer, the candidate
        // build's staging maps, the encoded snapshot) use mmap, so they are
        // returned to the OS on free without waiting for a trim (rc6 RSS spike).
        libc::mallopt(libc::M_MMAP_THRESHOLD, 64 * 1024);
        // Trim the heap top more eagerly when a large block is freed.
        libc::mallopt(libc::M_TRIM_THRESHOLD, 64 * 1024);
    }
}

/// Stack size for the daemon's long-lived control threads. The default 8 MiB is
/// virtual and mostly untouched, but an explicit smaller size keeps the resident
/// stack pages bounded (rc6 RSS spike).
const CONTROL_STACK: usize = 1 << 20;
/// Stack size for a per-connection thread.
const CONNECTION_STACK: usize = 512 << 10;

/// Spawn a named thread with an explicit stack size. A spawn failure is ignored
/// (the daemon degrades rather than aborting).
fn spawn_thread<F: FnOnce() + Send + 'static>(name: &str, stack: usize, f: F) {
    let _ = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack)
        .spawn(f);
}

/// Bound the rayon global pool used by the scan and the parallel search.
///
/// The scan is I/O-bound and scales only ~1.4x from one to sixteen cores (F8),
/// so a small pool keeps the throughput while cutting the resident thread
/// stacks. Override with `OMAVRYTHING_SCAN_THREADS`; the default is
/// `min(cores, 4)`.
fn tune_scan_pool() {
    let threads = std::env::var("OMAVRYTHING_SCAN_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(4)
        });
    if !omavrything_core::tune_scan_pool(threads) {
        debug!("scan pool already built; OMAVRYTHING_SCAN_THREADS ignored");
    }
}

/// The resolved daemon configuration: command-line flags override the state
/// config, which overrides the base config file, which overrides the built-in
/// defaults.
struct Args {
    roots: Vec<PathBuf>,
    auto: bool,
    excludes: Vec<String>,
    socket: PathBuf,
    watch: bool,
    scan_interval: u64,
    snapshot: Option<PathBuf>,
    snapshot_interval: u64,
    evict_idle_secs: u64,
    dbus: bool,
    ntfs: bool,
    metadata: bool,
    log_level: LogLevel,
    background_opacity: f64,
    /// Whether the n-gram candidate index is built (F10).
    candidate_index: bool,
    /// Whether repeated queries are served from the memoization cache (F7).
    query_cache: bool,
    /// The persisted runtime overrides, carried for `SetConfig` persistence.
    state: FileConfig,
    /// Where to persist `state`; `None` disables persistence.
    state_path: Option<PathBuf>,
}

/// Raw command-line options. `None` means "not specified on the command line",
/// so the config file (or the built-in default) can supply the value.
#[derive(Default)]
struct RawArgs {
    roots: Vec<PathBuf>,
    auto: bool,
    excludes: Vec<String>,
    socket: Option<PathBuf>,
    watch: Option<bool>,
    scan_interval: Option<u64>,
    snapshot: Option<PathBuf>,
    no_snapshot: bool,
    snapshot_interval: Option<u64>,
    dbus: Option<bool>,
    ntfs: Option<bool>,
    no_metadata: bool,
    config: Option<PathBuf>,
    state_config: Option<PathBuf>,
    no_config: bool,
    log_level: Option<String>,
    candidate_index: Option<bool>,
    no_query_cache: bool,
}

impl Args {
    /// Parse the command line, load the base and state configs, and merge them.
    fn parse() -> Result<Self, String> {
        let raw = RawArgs::parse()?;
        let base = load_file_config(&raw)?;
        let state_path = resolve_state_path(&raw);
        let state = load_state_config(&state_path)?;
        Self::resolve(raw, base, state, state_path)
    }

    /// Merge the command line over the state config over the base config over
    /// the defaults.
    fn resolve(
        raw: RawArgs,
        base: FileConfig,
        state: FileConfig,
        state_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let log_level = match &raw.log_level {
            Some(level) => level.parse::<LogLevel>()?,
            None => state
                .log_level()
                .or_else(|| base.log_level())
                .unwrap_or_default(),
        };

        let auto = raw.auto || state.auto.or(base.auto).unwrap_or(false);
        let roots = if !raw.roots.is_empty() {
            raw.roots
        } else {
            state.roots.clone().or(base.roots).unwrap_or_default()
        };

        // Command-line excludes extend the state/base excludes.
        let mut excludes = state.excludes.clone().or(base.excludes).unwrap_or_default();
        excludes.extend(raw.excludes);

        let socket = raw
            .socket
            .or(state.socket.clone())
            .or(base.socket)
            .unwrap_or_else(default_socket_path);

        let no_snapshot = raw.no_snapshot
            || state.no_snapshot.unwrap_or(false)
            || base.no_snapshot.unwrap_or(false);
        let snapshot = if no_snapshot {
            None
        } else {
            raw.snapshot
                .or(state.snapshot.clone())
                .or(base.snapshot)
                .or_else(|| Some(default_snapshot_path()))
        };

        let scan_interval = raw
            .scan_interval
            .or(state.scan_interval)
            .or(base.scan_interval)
            .unwrap_or(900);
        let snapshot_interval = raw
            .snapshot_interval
            .or(state.snapshot_interval)
            .or(base.snapshot_interval)
            .unwrap_or(DEFAULT_SNAPSHOT_INTERVAL);
        let evict_idle_secs = state
            .evict_idle_secs
            .or(base.evict_idle_secs)
            .unwrap_or(EVICT_IDLE_SECS);
        let watch = raw.watch.or(state.watch).or(base.watch).unwrap_or(true);
        let dbus = raw.dbus.or(state.dbus).or(base.dbus).unwrap_or(true);
        let ntfs = raw.ntfs.or(state.ntfs).or(base.ntfs).unwrap_or(true);
        let metadata = if raw.no_metadata {
            false
        } else {
            state.metadata.or(base.metadata).unwrap_or(true)
        };
        let background_opacity = state
            .background_opacity
            .or(base.background_opacity)
            .unwrap_or(0.75);
        let candidate_index = raw
            .candidate_index
            .or(state.candidate_index)
            .or(base.candidate_index)
            .unwrap_or(true);
        let query_cache = !raw.no_query_cache;

        Ok(Self {
            roots,
            auto,
            excludes,
            socket,
            watch,
            scan_interval,
            snapshot,
            snapshot_interval,
            evict_idle_secs,
            dbus,
            ntfs,
            metadata,
            log_level,
            background_opacity,
            candidate_index,
            query_cache,
            state,
            state_path,
        })
    }

    /// Build the scan configuration. `--auto` discovers roots from the mount
    /// table; explicit roots take precedence; otherwise `$HOME` is indexed (a
    /// development convenience).
    fn scan_config(&self) -> ScanConfig {
        let roots = if self.auto {
            Vec::new()
        } else if self.roots.is_empty() {
            vec![
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/")),
            ]
        } else {
            self.roots.clone()
        };
        ScanConfig {
            roots,
            excludes: self.excludes.clone(),
            skip_pseudo: true,
            metadata: self.metadata,
        }
    }
}

impl RawArgs {
    fn parse() -> Result<Self, String> {
        let mut raw = RawArgs::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" => {
                    if let Some(value) = args.next() {
                        raw.roots.push(PathBuf::from(value));
                    }
                }
                "--auto" => raw.auto = true,
                "--exclude" => {
                    if let Some(value) = args.next() {
                        raw.excludes.push(value);
                    }
                }
                "--socket" => {
                    if let Some(value) = args.next() {
                        raw.socket = Some(PathBuf::from(value));
                    }
                }
                "--no-watch" => raw.watch = Some(false),
                "--scan-interval" => {
                    if let Some(value) = args.next() {
                        raw.scan_interval = Some(
                            value
                                .parse()
                                .map_err(|_| format!("invalid --scan-interval: {value}"))?,
                        );
                    }
                }
                "--snapshot" => {
                    if let Some(value) = args.next() {
                        raw.snapshot = Some(PathBuf::from(value));
                    }
                }
                "--no-snapshot" => raw.no_snapshot = true,
                "--snapshot-interval" => {
                    if let Some(value) = args.next() {
                        raw.snapshot_interval = Some(
                            value
                                .parse()
                                .map_err(|_| format!("invalid --snapshot-interval: {value}"))?,
                        );
                    }
                }
                "--no-dbus" => raw.dbus = Some(false),
                "--no-ntfs" => raw.ntfs = Some(false),
                "--no-metadata" => raw.no_metadata = true,
                "--config" => {
                    if let Some(value) = args.next() {
                        raw.config = Some(PathBuf::from(value));
                    }
                }
                "--state-config" => {
                    if let Some(value) = args.next() {
                        raw.state_config = Some(PathBuf::from(value));
                    }
                }
                "--no-config" => raw.no_config = true,
                "--no-candidate-index" => raw.candidate_index = Some(false),
                "--no-query-cache" => raw.no_query_cache = true,
                "--log-level" => {
                    if let Some(value) = args.next() {
                        raw.log_level = Some(value);
                    }
                }
                "-V" | "--version" => {
                    println!("omavrythingd {VERSION}");
                    std::process::exit(0);
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    print_help();
                    std::process::exit(2);
                }
            }
        }
        Ok(raw)
    }
}

/// Load the base configuration file: the `--config` path when given, otherwise
/// the default path when it exists. `--no-config` skips loading entirely.
fn load_file_config(raw: &RawArgs) -> Result<FileConfig, String> {
    if raw.no_config {
        return Ok(FileConfig::default());
    }
    match &raw.config {
        Some(path) => FileConfig::load(path),
        None => {
            let path = Path::new(DEFAULT_CONFIG_PATH);
            if path.exists() {
                FileConfig::load(path)
            } else {
                Ok(FileConfig::default())
            }
        }
    }
}

/// The state config path: `--state-config` when given, otherwise the default.
/// `--no-config` disables persistence.
fn resolve_state_path(raw: &RawArgs) -> Option<PathBuf> {
    if raw.no_config {
        return None;
    }
    Some(
        raw.state_config
            .clone()
            .unwrap_or_else(default_state_config_path),
    )
}

/// Load the state config, or an empty one when it does not exist.
fn load_state_config(path: &Option<PathBuf>) -> Result<FileConfig, String> {
    match path {
        Some(p) if p.exists() => FileConfig::load(p),
        _ => Ok(FileConfig::default()),
    }
}

/// The default socket path: `/run/omavrything` for the root system service,
/// the per-user runtime directory otherwise.
fn default_socket_path() -> PathBuf {
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/run/omavrything/omavrything.sock")
    } else {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("omavrything.sock"),
            None => PathBuf::from("/tmp/omavrything.sock"),
        }
    }
}

/// The default snapshot path: `/var/lib` for the root system service, the XDG
/// state directory for a user-run daemon.
fn default_snapshot_path() -> PathBuf {
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/var/lib/omavrything/index.snapshot")
    } else {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("omavrything/index.snapshot")
    }
}

/// The default state config path: `/var/lib` for the root system service, the
/// XDG state directory for a user-run daemon.
fn default_state_config_path() -> PathBuf {
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        PathBuf::from("/var/lib/omavrything/config.toml")
    } else {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("omavrything/config.toml")
    }
}

fn print_help() {
    println!(
        "omavrythingd {VERSION}\n\
         \n\
         Usage: omavrythingd [--config <path>] [--root <dir>]... [--auto] [--exclude <glob>]...\n\
         \x20                  [--no-watch] [--scan-interval <secs>] [--socket <path>]\n\
         \x20                  [--snapshot <path>] [--no-snapshot] [--snapshot-interval <secs>]\n\
         \x20                  [--no-dbus] [--no-ntfs] [--no-metadata] [--log-level <level>]\n\
         \x20                  [--no-candidate-index] [--no-query-cache]\n\
         \n\
         Options:\n\
           --config <path>       Configuration file (default: {DEFAULT_CONFIG_PATH})\n\
           --no-config           Ignore the configuration file\n\
           --root <dir>          Directory to index (repeatable; default: $HOME)\n\
           --auto                Discover roots from the mount table (skip pseudo-fs)\n\
           --exclude <glob>      Exclude glob, e.g. '**/.git/**' (repeatable)\n\
           --no-watch            Disable inotify live updates\n\
           --scan-interval <s>   Idle-gated rescan interval in seconds (0 disables; default 900)\n\
           --socket <path>       Unix socket path (default: /run/omavrything/omavrything.sock as root,\n\
         \x20                       else $XDG_RUNTIME_DIR/omavrything.sock)\n\
           --snapshot <path>     Snapshot path (default: /var/lib/omavrything/index.snapshot as root,\n\
         \x20                       else $XDG_STATE_HOME/omavrything/index.snapshot)\n\
           --no-snapshot         Disable snapshot persistence\n\
           --snapshot-interval <s>  Snapshot write interval in seconds (0 disables; default 300)\n\
           --no-dbus             Disable the D-Bus control interface\n\
           --no-ntfs             Disable NTFS partition detection and indexing\n\
           --no-metadata         Skip per-file size/mtime stats (names-only; faster scan)\n\
           --no-candidate-index  Disable the n-gram candidate index (linear scan; bounds memory)\n\
           --no-query-cache      Disable the query-result memoization cache (cold-path measurement)\n\
           --log-level <level>   Log level: error, warn, info, debug, trace (default: error)\n\
           -V, --version         Show the version\n\
           -h, --help            Show this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_lock_is_exclusive() {
        let dir =
            std::env::temp_dir().join(format!("omavrything-singleton-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("omavrything.sock");

        let first = acquire_singleton(&sock).unwrap();
        assert!(acquire_singleton(&sock).is_err());
        drop(first);
        // Once released, the lock can be re-acquired.
        assert!(acquire_singleton(&sock).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_line_bounded_enforces_deadline() {
        let mut reader = std::io::Cursor::new(b"no newline here".to_vec());
        let err = read_line_bounded(&mut reader, 1024, Duration::ZERO).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn read_line_bounded_rejects_overlong_lines() {
        let mut reader = std::io::Cursor::new(b"aaaaaaaaaa\n".to_vec());
        let err = read_line_bounded(&mut reader, 4, Duration::from_secs(30)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_line_bounded_reads_a_line() {
        let mut reader = std::io::Cursor::new(b"hello\nworld\n".to_vec());
        let line = read_line_bounded(&mut reader, 1024, Duration::from_secs(30))
            .unwrap()
            .unwrap();
        assert_eq!(line, "hello");
    }

    #[test]
    fn contain_panic_turns_a_panic_into_an_error() {
        // The NTFS startup pass uses this so that a malformed volume cannot unwind
        // the thread and stop the remaining partitions from being indexed.
        assert_eq!(contain_panic(|| 7), Ok(7));
        assert!(contain_panic(|| panic!("malformed volume")).is_err());
    }
}

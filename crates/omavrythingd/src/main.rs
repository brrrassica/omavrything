//! The `omavrythingd` index daemon.
//!
//! Discovers mounted filesystems (or uses explicit roots), scans them in
//! parallel, keeps the index current with inotify, runs idle-gated periodic
//! rescans, and persists a snapshot for warm starts. Queries are served as
//! newline-delimited JSON over a Unix socket; control and status are exposed on
//! D-Bus.

mod config;
mod dbus;
mod peer;
mod polkit;

use std::ffi::CString;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use omavrything_core::VERSION;
use omavrything_core::idle::{IdleMonitor, IdleThresholds, RescanScheduler};
use omavrything_core::index::Index;
use omavrything_core::live::{self, FsEvent};
use omavrything_core::protocol::{Method, Request, Response, ResultItem};
use omavrything_core::scan::{ScanConfig, rescan_into_index, scan};
use omavrything_core::snapshot::{self, RootIdentity, WarmStart};
use omavrything_core::watch::Watcher;
use tokio::sync::mpsc::UnboundedSender;

use config::SharedConfig;
use dbus::IndexEvent;

/// Maximum number of concurrent client connections.
const MAX_CONNECTIONS: usize = 64;
/// Maximum length of a single request line, in bytes.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Per-connection read timeout.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the watcher blocks waiting for events before checking pending moves.
const WATCH_POLL: Duration = Duration::from_millis(500);
/// Default interval between periodic snapshot writes.
const DEFAULT_SNAPSHOT_INTERVAL: u64 = 300;
/// The group that may use the system socket.
const SOCKET_GROUP: &str = "omavrything";

fn main() {
    let args = Args::parse();

    eprintln!("omavrythingd {VERSION}");
    let scan_config = args.scan_config();
    let started = Instant::now();

    let (index, report, identities, outcome) = match &args.snapshot {
        Some(path) => {
            let warm = snapshot::warm_start(&scan_config, path);
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

    match outcome {
        Some(WarmStart::Snapshot) => eprintln!(
            "warm-started from snapshot: {} entries in {:?}",
            index.len(),
            started.elapsed()
        ),
        Some(WarmStart::Partial { rescanned }) => eprintln!(
            "warm-started from snapshot: {} entries; rescanned {rescanned} root(s) in {:?}",
            index.len(),
            started.elapsed()
        ),
        Some(WarmStart::ColdScan) => eprintln!(
            "no usable snapshot; cold scan: {} entries across {} root(s) in {:?} ({} skipped, {} unreadable)",
            report.entries,
            report.roots,
            started.elapsed(),
            report.skipped,
            report.errors
        ),
        None => eprintln!(
            "indexed {} entries across {} root(s) in {:?} ({} skipped, {} unreadable)",
            report.entries,
            report.roots,
            started.elapsed(),
            report.skipped,
            report.errors
        ),
    }
    for root in index.roots() {
        eprintln!("  root {} ({:?})", root.path, root.fs_kind);
    }

    let index = Arc::new(RwLock::new(index));
    let config = Arc::new(SharedConfig::new(
        scan_config,
        args.scan_interval,
        args.snapshot_interval,
        args.snapshot.clone(),
        args.watch,
    ));
    let identities = Arc::new(RwLock::new(identities));

    // Index events are forwarded to the D-Bus signal task. If D-Bus is
    // disabled the receiver is dropped and sends are ignored.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<IndexEvent>();

    // Block SIGTERM/SIGINT before spawning threads so they inherit the mask and
    // a dedicated thread can perform a clean snapshot write on shutdown.
    install_shutdown_handler(
        Arc::clone(&index),
        Arc::clone(&identities),
        args.snapshot.clone(),
    );

    if args.watch {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        std::thread::spawn(move || run_watcher(index, config, events));
    }

    if args.scan_interval > 0 {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        std::thread::spawn(move || run_scheduler(index, config, events));
    }

    if let Some(path) = &args.snapshot {
        if args.snapshot_interval > 0 {
            let index = Arc::clone(&index);
            let identities = Arc::clone(&identities);
            let config = Arc::clone(&config);
            let path = path.clone();
            std::thread::spawn(move || run_snapshot(index, identities, config, path));
        }
    }

    if args.dbus {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        let events = event_tx.clone();
        std::thread::spawn(move || dbus::run(index, config, started, event_rx, events));
    }

    // A stale socket from a previous run would make bind() fail.
    if let Some(parent) = args.socket.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!("failed to create {}: {err}", parent.display());
        }
    }
    let _ = std::fs::remove_file(&args.socket);
    let listener = match UnixListener::bind(&args.socket) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {}: {err}", args.socket.display());
            std::process::exit(1);
        }
    };
    if let Err(err) = prepare_socket(&args.socket) {
        eprintln!("warning: could not secure {}: {err}", args.socket.display());
    }
    eprintln!("listening on {}", args.socket.display());

    let allowed_gid = peer::group_gid(SOCKET_GROUP);
    if allowed_gid.is_none() {
        eprintln!("warning: group '{SOCKET_GROUP}' not found; relying on socket permissions");
    }

    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                match peer::peer_cred(&stream) {
                    Some(cred) if peer::peer_allowed(&cred, allowed_gid) => {}
                    Some(cred) => {
                        eprintln!(
                            "rejecting connection from uid {} gid {} pid {}",
                            cred.uid, cred.gid, cred.pid
                        );
                        drop(stream);
                        continue;
                    }
                    None => {
                        eprintln!("rejecting connection: could not read peer credentials");
                        drop(stream);
                        continue;
                    }
                }
                if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    eprintln!("connection limit reached; dropping client");
                    drop(stream);
                    continue;
                }
                active.fetch_add(1, Ordering::Relaxed);
                let index = Arc::clone(&index);
                let active = Arc::clone(&active);
                std::thread::spawn(move || {
                    serve(stream, index);
                    active.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
}

/// Set the socket to mode `0660` and, when running as root, group
/// `omavrything`.
fn prepare_socket(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        if let Some(gid) = peer::group_gid(SOCKET_GROUP) {
            let c = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path has NUL"))?;
            // SAFETY: `c` is a valid NUL-terminated path; uid -1 leaves the
            // owner unchanged.
            let rc = unsafe { libc::chown(c.as_ptr(), !0 as libc::uid_t, gid) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Watch the indexed roots with inotify and apply events to the index.
fn run_watcher(
    index: Arc<RwLock<Index>>,
    config: Arc<SharedConfig>,
    events: UnboundedSender<IndexEvent>,
) {
    let mut watcher = match Watcher::new() {
        Ok(w) => w,
        Err(err) => {
            eprintln!("inotify init failed: {err}");
            return;
        }
    };

    let roots: Vec<PathBuf> = read_index(&index)
        .roots()
        .iter()
        .map(|r| PathBuf::from(&r.path))
        .collect();
    for root in &roots {
        if let Err(err) = watcher.watch_dir(root) {
            eprintln!("watch {} failed: {err}", root.display());
        }
    }
    let mut reported_failed = watcher.failed().len();
    if reported_failed > 0 {
        eprintln!(
            "warning: {reported_failed} directories could not be watched (max_user_watches exhausted?)"
        );
    }
    eprintln!("watching {} directories", watcher.watch_count());

    let mut buffer = [0u8; 8192];
    loop {
        match watcher.read_events(&mut buffer, WATCH_POLL) {
            Ok(batch) => {
                if batch.is_empty() {
                    continue;
                }
                let scan = config.scan();
                {
                    let mut idx = write_index(&index);
                    for event in &batch {
                        live::apply(&mut idx, event, &scan);
                    }
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
                    eprintln!(
                        "warning: {} more directories could not be watched (max_user_watches exhausted?)",
                        failed - reported_failed
                    );
                    reported_failed = failed;
                }
                let entries = read_index(&index).len() as u64;
                let _ = events.send(IndexEvent::Changed { entries });
            }
            Err(err) => {
                eprintln!("inotify read error: {err}");
                break;
            }
        }
    }
}

/// Periodically rescan the roots when the system is idle.
fn run_scheduler(
    index: Arc<RwLock<Index>>,
    config: Arc<SharedConfig>,
    events: UnboundedSender<IndexEvent>,
) {
    let mut monitor = IdleMonitor::new(IdleThresholds::default());
    let mut scheduler = RescanScheduler::new(Duration::from_secs(config.scan_interval().max(1)));
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let interval = config.scan_interval();
        if interval == 0 {
            continue;
        }
        scheduler.set_interval(Duration::from_secs(interval));
        let idle = monitor.is_idle();
        if scheduler.should_rescan(Instant::now(), idle) {
            eprintln!("idle: rescanning roots");
            let _ = events.send(IndexEvent::ScanProgress {
                phase: "start".into(),
                entries: 0,
            });
            let scan = config.scan();
            let (report, entries) = {
                let mut idx = write_index(&index);
                let report = rescan_into_index(&mut idx, &scan);
                (report, idx.len() as u64)
            };
            eprintln!(
                "rescan: +{} entries, -{} stale, {} unreadable",
                report.entries, report.skipped, report.errors
            );
            let _ = events.send(IndexEvent::ScanProgress {
                phase: "done".into(),
                entries,
            });
            let _ = events.send(IndexEvent::Changed { entries });
        }
    }
}

/// Periodically write a snapshot of the index.
fn run_snapshot(
    index: Arc<RwLock<Index>>,
    identities: Arc<RwLock<Vec<RootIdentity>>>,
    config: Arc<SharedConfig>,
    path: PathBuf,
) {
    loop {
        let interval = config.snapshot_interval();
        if interval == 0 {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        std::thread::sleep(Duration::from_secs(interval));
        let idx = read_index(&index);
        let ids = read_identities(&identities);
        if let Err(e) = snapshot::save(&idx, &ids, &path) {
            eprintln!("snapshot save failed: {e}");
        }
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
/// only writes a byte, and a dedicated thread performs the snapshot write.
fn install_shutdown_handler(
    index: Arc<RwLock<Index>>,
    identities: Arc<RwLock<Vec<RootIdentity>>>,
    snapshot: Option<PathBuf>,
) {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!(
            "failed to create shutdown pipe: {}",
            io::Error::last_os_error()
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
    std::thread::spawn(move || {
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
        eprintln!("shutdown signal received");
        if let Some(path) = &snapshot {
            let idx = read_index(&index);
            let ids = read_identities(&identities);
            match snapshot::save(&idx, &ids, path) {
                Ok(()) => eprintln!("snapshot written to {}", path.display()),
                Err(e) => eprintln!("snapshot save failed: {e}"),
            }
        }
        std::process::exit(0);
    });
}

/// Serve one client connection: one request per line, one response per line.
fn serve(stream: UnixStream, index: Arc<RwLock<Index>>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let read_half = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let mut writer = stream;
    let mut reader = BufReader::new(read_half);

    loop {
        let line = match read_line_bounded(&mut reader, MAX_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let _ = write_response(&mut writer, &Response::error(err.to_string()));
                continue;
            }
        };

        let response = match request.method {
            Method::Search => {
                let started = Instant::now();
                let guard = read_index(&index);
                let outcome = guard.search(
                    &request.q,
                    request.effective_limit(),
                    request.effective_offset(),
                );
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
/// EOF. A line longer than `max` is an error, so the connection is dropped
/// rather than buffering unbounded input.
fn read_line_bounded<R: BufRead>(reader: &mut R, max: usize) -> io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
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

/// Acquire a read lock, recovering from a poisoned lock rather than panicking.
fn read_index(index: &RwLock<Index>) -> RwLockReadGuard<'_, Index> {
    index.read().unwrap_or_else(|e| e.into_inner())
}

/// Acquire a write lock, recovering from a poisoned lock rather than panicking.
fn write_index(index: &RwLock<Index>) -> RwLockWriteGuard<'_, Index> {
    index.write().unwrap_or_else(|e| e.into_inner())
}

/// Clone the current root identities.
fn read_identities(identities: &RwLock<Vec<RootIdentity>>) -> Vec<RootIdentity> {
    identities.read().unwrap_or_else(|e| e.into_inner()).clone()
}

struct Args {
    roots: Vec<PathBuf>,
    auto: bool,
    excludes: Vec<String>,
    socket: PathBuf,
    watch: bool,
    scan_interval: u64,
    snapshot: Option<PathBuf>,
    snapshot_interval: u64,
    dbus: bool,
}

impl Args {
    fn parse() -> Self {
        let mut roots = Vec::new();
        let mut auto = false;
        let mut excludes = Vec::new();
        let mut socket = default_socket_path();
        let mut watch = true;
        let mut scan_interval = 900u64;
        let mut snapshot = Some(default_snapshot_path());
        let mut snapshot_interval = DEFAULT_SNAPSHOT_INTERVAL;
        let mut dbus = true;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" => {
                    if let Some(value) = args.next() {
                        roots.push(PathBuf::from(value));
                    }
                }
                "--auto" => auto = true,
                "--exclude" => {
                    if let Some(value) = args.next() {
                        excludes.push(value);
                    }
                }
                "--socket" => {
                    if let Some(value) = args.next() {
                        socket = PathBuf::from(value);
                    }
                }
                "--no-watch" => watch = false,
                "--scan-interval" => {
                    if let Some(value) = args.next() {
                        scan_interval = value.parse().unwrap_or(900);
                    }
                }
                "--snapshot" => {
                    if let Some(value) = args.next() {
                        snapshot = Some(PathBuf::from(value));
                    }
                }
                "--no-snapshot" => snapshot = None,
                "--snapshot-interval" => {
                    if let Some(value) = args.next() {
                        snapshot_interval = value.parse().unwrap_or(DEFAULT_SNAPSHOT_INTERVAL);
                    }
                }
                "--no-dbus" => dbus = false,
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

        Self {
            roots,
            auto,
            excludes,
            socket,
            watch,
            scan_interval,
            snapshot,
            snapshot_interval,
            dbus,
        }
    }

    /// Build the scan configuration. `--auto` discovers roots from the mount
    /// table; explicit `--root` flags take precedence; otherwise `$HOME` is
    /// indexed (a development convenience).
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
        }
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

fn print_help() {
    println!(
        "omavrythingd {VERSION}\n\
         \n\
         Usage: omavrythingd [--root <dir>]... [--auto] [--exclude <glob>]...\n\
         \x20                  [--no-watch] [--scan-interval <secs>] [--socket <path>]\n\
         \x20                  [--snapshot <path>] [--no-snapshot] [--snapshot-interval <secs>]\n\
         \x20                  [--no-dbus]\n\
         \n\
         Options:\n\
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
           -h, --help            Show this help"
    );
}

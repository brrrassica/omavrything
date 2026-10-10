//! The `omavrythingd` index daemon.
//!
//! M5: discovers mounted filesystems (or uses explicit roots), scans them in
//! parallel, keeps the index current with inotify, and runs idle-gated periodic
//! rescans. Queries are served as newline-delimited JSON over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use omavrything_core::VERSION;
use omavrything_core::idle::{IdleMonitor, IdleThresholds, RescanScheduler};
use omavrything_core::index::Index;
use omavrything_core::live::{self, FsEvent};
use omavrything_core::protocol::{Request, Response, ResultItem};
use omavrything_core::scan::{ScanConfig, rescan_into_index, scan};
use omavrything_core::watch::Watcher;

fn main() {
    let args = Args::parse();

    eprintln!("omavrythingd {VERSION}");
    let config = Arc::new(args.scan_config());
    let started = Instant::now();
    let (index, report) = scan(&config);
    eprintln!(
        "indexed {} entries across {} root(s) in {:?} ({} skipped)",
        report.entries,
        report.roots,
        started.elapsed(),
        report.skipped
    );
    for root in index.roots() {
        eprintln!("  root {} ({:?})", root.path, root.fs_kind);
    }

    let index = Arc::new(RwLock::new(index));

    if args.watch {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        std::thread::spawn(move || run_watcher(index, config));
    }

    if args.scan_interval > 0 {
        let index = Arc::clone(&index);
        let config = Arc::clone(&config);
        std::thread::spawn(move || run_scheduler(index, config, args.scan_interval));
    }

    // A stale socket from a previous run would make bind() fail.
    let _ = std::fs::remove_file(&args.socket);
    let listener = match UnixListener::bind(&args.socket) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {}: {err}", args.socket.display());
            std::process::exit(1);
        }
    };
    eprintln!("listening on {}", args.socket.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let index = Arc::clone(&index);
                std::thread::spawn(move || serve(stream, index));
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
}

/// Watch the indexed roots with inotify and apply events to the index.
fn run_watcher(index: Arc<RwLock<Index>>, config: Arc<ScanConfig>) {
    let mut watcher = match Watcher::new() {
        Ok(w) => w,
        Err(err) => {
            eprintln!("inotify init failed: {err}");
            return;
        }
    };

    let roots: Vec<PathBuf> = index
        .read()
        .unwrap()
        .roots()
        .iter()
        .map(|r| PathBuf::from(&r.path))
        .collect();
    for root in &roots {
        if let Err(err) = watcher.watch_dir(root) {
            eprintln!("watch {} failed: {err}", root.display());
        }
    }
    if !watcher.failed().is_empty() {
        eprintln!(
            "warning: {} directories could not be watched (max_user_watches exhausted?)",
            watcher.failed().len()
        );
    }
    eprintln!("watching {} directories", watcher.watch_count());

    let mut buffer = [0u8; 8192];
    loop {
        match watcher.read_events(&mut buffer) {
            Ok(events) => {
                if events.is_empty() {
                    continue;
                }
                {
                    let mut idx = index.write().unwrap();
                    for event in &events {
                        live::apply(&mut idx, event, &config);
                    }
                }
                // Watch newly created directories.
                for event in &events {
                    if let FsEvent::Created(path) = event {
                        if path.is_dir() {
                            let _ = watcher.watch_dir(path);
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("inotify read error: {err}");
                break;
            }
        }
    }
}

/// Periodically rescan the roots when the system is idle.
fn run_scheduler(index: Arc<RwLock<Index>>, config: Arc<ScanConfig>, interval_secs: u64) {
    let mut monitor = IdleMonitor::new(IdleThresholds::default());
    let mut scheduler = RescanScheduler::new(Duration::from_secs(interval_secs));
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let idle = monitor.is_idle();
        if scheduler.should_rescan(Instant::now(), idle) {
            eprintln!("idle: rescanning roots");
            let mut idx = index.write().unwrap();
            let report = rescan_into_index(&mut idx, &config);
            eprintln!(
                "rescan: +{} entries, -{} stale",
                report.entries, report.skipped
            );
        }
    }
}

/// Serve one client connection: one request per line, one response per line.
fn serve(stream: UnixStream, index: Arc<RwLock<Index>>) {
    let read_half = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let mut writer = stream;
    let reader = BufReader::new(read_half);

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    serde_json::json!({ "error": err.to_string() })
                );
                continue;
            }
        };

        let started = Instant::now();
        let response = {
            let guard = index.read().unwrap();
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
                total: outcome.total,
                results,
                took_ms: started.elapsed().as_millis() as u64,
            }
        };

        match serde_json::to_string(&response) {
            Ok(json) => {
                if writeln!(writer, "{json}").is_err() {
                    break;
                }
            }
            Err(err) => eprintln!("serialize error: {err}"),
        }
    }
}

struct Args {
    roots: Vec<PathBuf>,
    auto: bool,
    excludes: Vec<String>,
    socket: PathBuf,
    watch: bool,
    scan_interval: u64,
}

impl Args {
    fn parse() -> Self {
        let mut roots = Vec::new();
        let mut auto = false;
        let mut excludes = Vec::new();
        let mut socket = default_socket_path();
        let mut watch = true;
        let mut scan_interval = 900u64;

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

fn default_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("omavrything.sock"),
        None => PathBuf::from("/tmp/omavrything.sock"),
    }
}

fn print_help() {
    println!(
        "omavrythingd {VERSION}\n\
         \n\
         Usage: omavrythingd [--root <dir>]... [--auto] [--exclude <glob>]...\n\
         \x20                  [--no-watch] [--scan-interval <secs>] [--socket <path>]\n\
         \n\
         Options:\n\
           --root <dir>          Directory to index (repeatable; default: $HOME)\n\
           --auto                Discover roots from the mount table (skip pseudo-fs)\n\
           --exclude <glob>      Exclude glob, e.g. '**/.git/**' (repeatable)\n\
           --no-watch            Disable inotify live updates\n\
           --scan-interval <s>   Idle-gated rescan interval in seconds (0 disables; default 900)\n\
           --socket <path>       Unix socket path (default: $XDG_RUNTIME_DIR/omavrything.sock)\n\
           -h, --help            Show this help"
    );
}

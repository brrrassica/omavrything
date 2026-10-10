//! The `omavrythingd` index daemon.
//!
//! M2 walking skeleton: scans a root directory into a minimal in-memory index
//! and serves newline-delimited JSON queries over a Unix domain socket. The
//! production daemon (M3+) adds live updates, persistence, D-Bus, and NTFS.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use omavrything_core::VERSION;
use omavrything_core::index::Index;
use omavrything_core::protocol::{Request, Response, ResultItem};
use omavrything_core::scan::scan_into;

fn main() {
    let args = Args::parse();

    eprintln!("omavrythingd {VERSION}");
    eprintln!("scanning {} ...", args.root.display());
    let started = Instant::now();
    let mut index = Index::new();
    scan_into(&args.root, &mut index);
    eprintln!("indexed {} entries in {:?}", index.len(), started.elapsed());

    let index = Arc::new(RwLock::new(index));

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
    root: PathBuf,
    socket: PathBuf,
}

impl Args {
    fn parse() -> Self {
        let mut root = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut socket = default_socket_path();

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" => {
                    if let Some(value) = args.next() {
                        root = PathBuf::from(value);
                    }
                }
                "--socket" => {
                    if let Some(value) = args.next() {
                        socket = PathBuf::from(value);
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

        Self { root, socket }
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
         Usage: omavrythingd [--root <dir>] [--socket <path>]\n\
         \n\
         Options:\n\
           --root <dir>     Directory to index (default: $HOME)\n\
           --socket <path>  Unix socket path (default: $XDG_RUNTIME_DIR/omavrything.sock)\n\
           -h, --help       Show this help"
    );
}

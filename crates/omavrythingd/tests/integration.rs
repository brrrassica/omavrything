//! End-to-end test: start the daemon, query it over the Unix socket, and stop
//! it with SIGTERM.
//!
//! The daemon is spawned as a child process (the real binary, via
//! `CARGO_BIN_EXE_omavrythingd`) with D-Bus, NTFS, inotify, snapshots, and the
//! config file disabled, so the test is hermetic and needs no privileges.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omavrything_core::protocol::{IndexState, Method, Request, Response};

/// A daemon child process that is stopped and reaped on drop.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: `kill` with a valid pid and signal has no other preconditions.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omavrything-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_daemon(root: &Path, socket: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_omavrythingd"))
        .arg("--no-config")
        .arg("--root")
        .arg(root)
        .arg("--socket")
        .arg(socket)
        .arg("--no-dbus")
        .arg("--no-ntfs")
        .arg("--no-watch")
        .arg("--no-snapshot")
        .arg("--scan-interval")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn omavrythingd");
    Daemon { child }
}

/// Wait until the daemon accepts connections on `socket`.
fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not start listening on {}", socket.display());
}

fn search(socket: &Path, query: &str) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect");
    let request = Request {
        method: Method::Search,
        q: query.to_string(),
        limit: Some(10),
        offset: Some(0),
    };
    writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).expect("parse response")
}

#[test]
fn daemon_indexes_and_answers_queries() {
    let dir = temp_dir("query");
    let root = dir.join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("report-2024.txt"), b"hello").unwrap();
    std::fs::write(root.join("sub").join("notes.md"), b"world").unwrap();

    let socket = dir.join("omavrything.sock");
    let _daemon = start_daemon(&root, &socket);
    wait_for_socket(&socket);

    let response = search(&socket, "report");
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(response.total, 1);
    assert!(response.results[0].path.ends_with("report-2024.txt"));

    // A query with no matches returns an empty, non-error response.
    let empty = search(&socket, "no-such-file-xyz");
    assert!(empty.error.is_none());
    assert_eq!(empty.total, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn daemon_evicts_when_idle_and_still_answers() {
    let dir = temp_dir("evict");
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("report-2024.txt"), b"hello").unwrap();

    let socket = dir.join("omavrything.sock");
    let snapshot = dir.join("index.snapshot");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "roots = [\"{}\"]\nsocket = \"{}\"\nsnapshot = \"{}\"\n\
             evict_idle_secs = 2\nsnapshot_interval = 0\nwatch = false\n\
             dbus = false\nntfs = false\n",
            root.display(),
            socket.display(),
            snapshot.display()
        ),
    )
    .unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_omavrythingd"))
        .arg("--config")
        .arg(&config)
        .arg("--no-dbus")
        .arg("--no-ntfs")
        .arg("--no-watch")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn omavrythingd");
    let _daemon = Daemon { child };
    wait_for_socket(&socket);

    // The first query is served from the owned index.
    assert_eq!(search(&socket, "report").total, 1);

    // The idle evictor writes the snapshot and evicts the owned index.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !snapshot.exists() {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        snapshot.exists(),
        "the idle evictor should have written the snapshot"
    );

    // The first query after eviction reports `loading` and starts a background
    // materialization; it does not scan the image.
    let after = search(&socket, "report");
    assert!(after.error.is_none(), "{:?}", after.error);
    assert_eq!(after.state, Some(IndexState::Loading));
    assert_eq!(after.total, 0);

    // Once the background materialization lands, queries are served from the
    // owned index again.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut served = search(&socket, "report");
    while Instant::now() < deadline && served.total == 0 {
        std::thread::sleep(Duration::from_millis(100));
        served = search(&socket, "report");
    }
    assert_eq!(served.total, 1);
    assert!(served.results[0].path.ends_with("report-2024.txt"));

    let _ = std::fs::remove_dir_all(&dir);
}

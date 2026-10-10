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

use omavrything_core::protocol::{Method, Request, Response};

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

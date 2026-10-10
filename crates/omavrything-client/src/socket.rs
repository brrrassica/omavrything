//! Client for the daemon's Unix-socket search protocol.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use omavrything_core::protocol::{Method, Request, Response};

/// A client for the newline-delimited JSON search protocol.
pub struct SocketClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl SocketClient {
    /// Connect to the daemon at `path`.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    /// Connect to the daemon, preferring the system socket and falling back to
    /// the per-user runtime socket when the system socket is absent or
    /// inaccessible (e.g. the caller is not in the `omavrything` group).
    pub fn connect_default() -> std::io::Result<Self> {
        let system = PathBuf::from("/run/omavrything/omavrything.sock");
        match Self::connect(&system) {
            Ok(client) => Ok(client),
            Err(system_err) => match Self::connect(&user_socket_path()) {
                Ok(client) => Ok(client),
                Err(_) => Err(system_err),
            },
        }
    }

    /// Run a search and return the response.
    pub fn search(
        &mut self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> std::io::Result<Response> {
        let request = Request {
            method: Method::Search,
            q: query.to_string(),
            limit: Some(limit),
            offset: Some(offset),
        };
        let line = serde_json::to_string(&request).map_err(std::io::Error::other)?;
        writeln!(self.stream, "{line}")?;

        let mut response_line = String::new();
        self.reader.read_line(&mut response_line)?;
        serde_json::from_str(&response_line).map_err(std::io::Error::other)
    }
}

/// The default socket path: the system socket when present, otherwise the
/// per-user runtime socket.
pub fn default_socket_path() -> PathBuf {
    let system = PathBuf::from("/run/omavrything/omavrything.sock");
    if system.exists() {
        return system;
    }
    user_socket_path()
}

/// The per-user runtime socket path.
fn user_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("omavrything.sock"),
        None => PathBuf::from("/tmp/omavrything.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omavrything_core::protocol::{Request, Response, ResultItem};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn search_round_trips_over_a_socket() {
        let dir = std::env::temp_dir().join(format!("omavrything-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(request.q, "report");
            let response = Response {
                results: vec![ResultItem {
                    name: "report.txt".into(),
                    path: "/tmp/report.txt".into(),
                    size: 5,
                    mtime: 0,
                    is_dir: false,
                }],
                total: 1,
                took_ms: 1,
                error: None,
            };
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });

        let mut client = SocketClient::connect(&path).unwrap();
        let response = client.search("report", 10, 0).unwrap();
        assert_eq!(response.total, 1);
        assert_eq!(response.results[0].name, "report.txt");
        server.join().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}

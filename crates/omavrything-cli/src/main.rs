//! The `omavry` command-line client for `omavrything`.
//!
//! M2 walking skeleton: a `search` subcommand that queries the daemon over the
//! Unix socket. The full CLI (status, rescan, config, mount) arrives in M10.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use omavrything_core::VERSION;
use omavrything_core::protocol::{Request, Response};

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());

    match command.as_str() {
        "search" => {
            let query = args.collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                eprintln!("usage: omavry search <query>");
                std::process::exit(2);
            }
            std::process::exit(search(&query));
        }
        "help" | "-h" | "--help" => print_help(),
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn search(query: &str) -> i32 {
    let socket = default_socket_path();
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("cannot connect to {}: {err}", socket.display());
            eprintln!("is omavrythingd running?");
            return 1;
        }
    };

    let request = Request {
        q: query.to_string(),
        limit: Some(50),
    };
    let line = serde_json::to_string(&request).unwrap();
    if writeln!(stream, "{line}").is_err() {
        eprintln!("failed to send request");
        return 1;
    }

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    if reader.read_line(&mut response_line).is_err() {
        eprintln!("failed to read response");
        return 1;
    }

    let response: Response = match serde_json::from_str(&response_line) {
        Ok(response) => response,
        Err(err) => {
            eprintln!("invalid response: {err}");
            return 1;
        }
    };

    for item in &response.results {
        println!("{}", item.path);
    }
    eprintln!("{} result(s) in {} ms", response.total, response.took_ms);
    0
}

fn default_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("omavrything.sock"),
        None => PathBuf::from("/tmp/omavrything.sock"),
    }
}

fn print_help() {
    println!(
        "omavry {VERSION}\n\
         \n\
         Usage: omavry <command> [args]\n\
         \n\
         Commands:\n\
           search <query>   Search the index\n\
           help             Show this help"
    );
}

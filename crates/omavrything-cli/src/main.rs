//! The `omavry` command-line client for `omavrything`.
//!
//! Uses the shared `omavrything-client` library: `search` over the Unix socket
//! and `status`/`config`/`rescan`/`mount` over D-Bus. Every subcommand supports
//! a human-readable output and, where it makes sense, a `--json` mode for
//! scripting.

use omavrything_client::{DbusClient, SocketClient};
use omavrything_core::VERSION;

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    let rest: Vec<String> = args.collect();

    match command.as_str() {
        "search" => {
            let json = rest.iter().any(|a| a == "--json");
            let query = rest
                .iter()
                .filter(|a| a.as_str() != "--json")
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            if query.trim().is_empty() {
                eprintln!("usage: omavry search [--json] <query>");
                std::process::exit(2);
            }
            std::process::exit(search(&query, json));
        }
        "status" => {
            let json = rest.iter().any(|a| a == "--json");
            std::process::exit(status(json));
        }
        "rescan" => {
            let root = parse_root(&rest);
            std::process::exit(rescan(root.as_deref()));
        }
        "volumes" => {
            let json = rest.iter().any(|a| a == "--json");
            std::process::exit(volumes(json));
        }
        "mount" => match rest.iter().find(|a| !a.starts_with("--")) {
            Some(uuid) => std::process::exit(mount(uuid)),
            None => {
                eprintln!("usage: omavry mount <uuid>");
                std::process::exit(2);
            }
        },
        "mount-volume" => match rest.iter().find(|a| !a.starts_with("--")) {
            Some(device) => std::process::exit(mount_volume(device)),
            None => {
                eprintln!("usage: omavry mount-volume <device>");
                std::process::exit(2);
            }
        },
        "unmount-volume" => match rest.iter().find(|a| !a.starts_with("--")) {
            Some(device) => std::process::exit(unmount_volume(device)),
            None => {
                eprintln!("usage: omavry unmount-volume <device>");
                std::process::exit(2);
            }
        },
        "config" => {
            let mut it = rest.iter();
            let sub = it.next().map(String::as_str).unwrap_or_default();
            match sub {
                "get" => {
                    let json = it.any(|a| a == "--json");
                    std::process::exit(config_get(json));
                }
                "set" => match (it.next(), it.next()) {
                    (Some(key), Some(value)) => std::process::exit(config_set(key, value)),
                    _ => {
                        eprintln!("usage: omavry config set <key> <value>");
                        std::process::exit(2);
                    }
                },
                _ => {
                    eprintln!(
                        "usage: omavry config get [--json] | omavry config set <key> <value>"
                    );
                    std::process::exit(2);
                }
            }
        }
        "help" | "-h" | "--help" => print_help(),
        "-V" | "--version" => {
            println!("omavry {VERSION}");
            std::process::exit(0);
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

/// Parse `--root <path>` or `--root=<path>` from the argument list.
fn parse_root(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--root" {
            return it.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--root=") {
            return Some(value.to_string());
        }
    }
    None
}

/// Write one line to stdout, exiting quietly with 0 on a broken pipe.
///
/// `omavry search … | head` closes the pipe after `head` exits; the default
/// Rust behaviour is to panic with "failed printing to stdout: Broken pipe".
/// Standard Unix tools exit 0 in that situation, so we do the same.
fn print_line(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").is_err() {
        std::process::exit(0);
    }
}

fn search(query: &str, json: bool) -> i32 {
    let mut client = match SocketClient::connect_default() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("cannot connect to the daemon: {err}");
            eprintln!("is omavrythingd running?");
            return 1;
        }
    };

    // The daemon evicts the index when idle; a query while evicted returns
    // `state: "loading"` and starts a background materialization. Retry briefly
    // so the command returns results instead of a misleading empty page,
    // matching the overlay's reload behavior.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let response = loop {
        let response = match client.search(query, 50, 0) {
            Ok(response) => response,
            Err(err) => {
                eprintln!("search failed: {err}");
                return 1;
            }
        };
        if !response.is_loading() || std::time::Instant::now() >= deadline {
            break response;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };

    if let Some(err) = &response.error {
        eprintln!("error: {err}");
        return 1;
    }

    if json {
        match serde_json::to_string(&response) {
            Ok(text) => print_line(&text),
            Err(err) => {
                eprintln!("search failed: {err}");
                return 1;
            }
        }
    } else {
        for item in &response.results {
            print_line(&item.path);
        }
        eprintln!(
            "{} of {} result(s) in {} ms",
            response.results.len(),
            response.total,
            response.took_ms
        );
    }
    0
}

fn status(json: bool) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.status().await {
                Ok(status) => {
                    if json {
                        match serde_json::to_string(&status) {
                            Ok(text) => print_line(&text),
                            Err(err) => {
                                eprintln!("status failed: {err}");
                                return 1;
                            }
                        }
                    } else {
                        let mut keys: Vec<&String> = status.keys().collect();
                        keys.sort();
                        for key in keys {
                            print_line(&format!("{key}: {}", status[key]));
                        }
                    }
                    0
                }
                Err(err) => {
                    eprintln!("status failed: {err}");
                    1
                }
            }
        })
    })
}

fn rescan(root: Option<&str>) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.rescan(root.unwrap_or("")).await {
                Ok(added) => {
                    match root {
                        Some(root) => print_line(&format!("{added} entries added under {root}")),
                        None => print_line(&format!("{added} entries added")),
                    }
                    0
                }
                Err(err) => {
                    eprintln!("rescan failed: {err}");
                    1
                }
            }
        })
    })
}

fn volumes(json: bool) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.volumes().await {
                Ok(volumes) => {
                    if json {
                        match serde_json::to_string(&volumes) {
                            Ok(text) => print_line(&text),
                            Err(err) => {
                                eprintln!("volumes failed: {err}");
                                return 1;
                            }
                        }
                    } else {
                        for volume in &volumes {
                            let label = volume.get("label").map(String::as_str).unwrap_or("");
                            let fstype = volume.get("fstype").map(String::as_str).unwrap_or("");
                            let status = volume.get("status").map(String::as_str).unwrap_or("");
                            let device = volume.get("device").map(String::as_str).unwrap_or("");
                            let name = if label.is_empty() { device } else { label };
                            print_line(&format!("{name} ({fstype}) {status}"));
                        }
                    }
                    0
                }
                Err(err) => {
                    eprintln!("volumes failed: {err}");
                    1
                }
            }
        })
    })
}

fn mount_volume(device: &str) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.mount_volume(device).await {
                Ok(mount_point) => {
                    print_line(&mount_point);
                    0
                }
                Err(err) => {
                    eprintln!("mount failed: {err}");
                    1
                }
            }
        })
    })
}

fn unmount_volume(device: &str) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.unmount_volume(device).await {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("unmount failed: {err}");
                    1
                }
            }
        })
    })
}

fn mount(uuid: &str) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.mount_ntfs(uuid).await {
                Ok(mount_point) => {
                    print_line(&mount_point);
                    0
                }
                Err(err) => {
                    eprintln!("mount failed: {err}");
                    1
                }
            }
        })
    })
}

fn config_get(json: bool) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.config().await {
                Ok(map) => {
                    if json {
                        match serde_json::to_string(&map) {
                            Ok(text) => print_line(&text),
                            Err(err) => {
                                eprintln!("config get failed: {err}");
                                return 1;
                            }
                        }
                    } else {
                        let mut keys: Vec<&String> = map.keys().collect();
                        keys.sort();
                        for key in keys {
                            print_line(&format!("{key}: {}", map[key]));
                        }
                    }
                    0
                }
                Err(err) => {
                    eprintln!("config get failed: {err}");
                    1
                }
            }
        })
    })
}

fn config_set(key: &str, value: &str) -> i32 {
    with_runtime(|runtime| {
        runtime.block_on(async {
            let client = match DbusClient::connect().await {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("cannot connect to the D-Bus interface: {err}");
                    return 1;
                }
            };
            match client.set_config(key, value).await {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("config set failed: {err}");
                    1
                }
            }
        })
    })
}

/// Build a current-thread runtime and run `f` on it.
fn with_runtime<F: FnOnce(&tokio::runtime::Runtime) -> i32>(f: F) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to start runtime: {err}");
            return 1;
        }
    };
    f(&runtime)
}

fn print_help() {
    print_line(&format!(
        "omavry {VERSION}\n\
         \n\
         Usage: omavry <command> [args]\n\
         \n\
         Commands:\n\
           search [--json] <query>   Search the index (Unix socket)\n\
           status [--json]           Show daemon status (D-Bus)\n\
           rescan [--root <path>]    Rescan all roots or one root (D-Bus, polkit)\n\
           volumes [--json]          List detected volumes and their status (D-Bus)\n\
           mount <uuid>              Mount and index an NTFS partition (D-Bus, polkit)\n\
           mount-volume <device>     Mount and index any volume (D-Bus, polkit)\n\
           unmount-volume <device>   Unmount a volume and drop its index (D-Bus, polkit)\n\
           config get [--json]       Show the daemon configuration (D-Bus)\n\
           config set <key> <val>    Set a configuration value (D-Bus, polkit)\n\
           help                      Show this help\n\
          \n\
          Options:\n\
           -V, --version             Show the version\n\
           -h, --help                Show this help"
    ));
}

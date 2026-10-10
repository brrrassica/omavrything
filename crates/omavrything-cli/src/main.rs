//! The `omavry` command-line client for `omavrything`.
//!
//! Uses the shared `omavrything-client` library: `search` over the Unix socket
//! and `status`/`config` over D-Bus. The remaining subcommands (rescan, mount)
//! arrive in M10.

use omavrything_client::{DbusClient, SocketClient};
use omavrything_core::VERSION;

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
        "status" => std::process::exit(status()),
        "config" => {
            let sub = args.next().unwrap_or_default();
            match sub.as_str() {
                "get" => {
                    let json = args.any(|a| a == "--json");
                    std::process::exit(config_get(json));
                }
                "set" => match (args.next(), args.next()) {
                    (Some(key), Some(value)) => std::process::exit(config_set(&key, &value)),
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
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn search(query: &str) -> i32 {
    let mut client = match SocketClient::connect_default() {
        Ok(client) => client,
        Err(err) => {
            eprintln!("cannot connect to the daemon: {err}");
            eprintln!("is omavrythingd running?");
            return 1;
        }
    };

    let response = match client.search(query, 50, 0) {
        Ok(response) => response,
        Err(err) => {
            eprintln!("search failed: {err}");
            return 1;
        }
    };

    if let Some(err) = &response.error {
        eprintln!("error: {err}");
        return 1;
    }

    for item in &response.results {
        println!("{}", item.path);
    }
    eprintln!(
        "{} of {} result(s) in {} ms",
        response.results.len(),
        response.total,
        response.took_ms
    );
    0
}

fn status() -> i32 {
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
                    let mut keys: Vec<&String> = status.keys().collect();
                    keys.sort();
                    for key in keys {
                        println!("{key}: {}", status[key]);
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
                            Ok(text) => println!("{text}"),
                            Err(err) => {
                                eprintln!("config get failed: {err}");
                                return 1;
                            }
                        }
                    } else {
                        let mut keys: Vec<&String> = map.keys().collect();
                        keys.sort();
                        for key in keys {
                            println!("{key}: {}", map[key]);
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
    println!(
        "omavry {VERSION}\n\
         \n\
         Usage: omavry <command> [args]\n\
         \n\
         Commands:\n\
           search <query>          Search the index (Unix socket)\n\
           status                  Show daemon status (D-Bus)\n\
           config get [--json]     Show the daemon configuration (D-Bus)\n\
           config set <key> <val>  Set a configuration value (D-Bus, polkit)\n\
           help                    Show this help"
    );
}

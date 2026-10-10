//! Structured logging for the daemon.
//!
//! Events are emitted to stderr through `tracing` in a stable, greppable
//! `TIMESTAMP LEVEL target: message field=value` format that journald captures
//! and `journalctl -o json` can parse. The level is taken from the
//! `OMAVRYTHING_LOG` environment variable when set, otherwise from the
//! `log_level` configuration key (default `info`). The level can be changed at
//! runtime with [`set_level`] (used by the D-Bus `SetConfig` method).

use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;

/// A log level, ordered from least to most verbose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// The lowercase name used in configuration and the environment.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => Err(format!(
                "invalid log level: {other} (expected error, warn, info, debug, or trace)"
            )),
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Install the global tracing subscriber.
///
/// The `OMAVRYTHING_LOG` environment variable, when set, overrides `level`; it
/// accepts the same syntax as `RUST_LOG` (e.g. `debug` or
/// `omavrythingd=debug,zbus=warn`). A second call is a no-op, so this is safe to
/// call from tests or a re-exec.
/// The reload handle for the active filter, set by [`init`].
static RELOAD: OnceLock<reload::Handle<EnvFilter, tracing_subscriber::Registry>> = OnceLock::new();

pub fn init(level: LogLevel) {
    let spec = std::env::var("OMAVRYTHING_LOG").unwrap_or_else(|_| level.as_str().to_string());
    let filter = EnvFilter::try_new(&spec).unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter, handle) = reload::Layer::new(filter);
    // `try_init` fails if a subscriber is already installed; that is fine.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_target(true),
        )
        .try_init();
    let _ = RELOAD.set(handle);
}

/// Change the active log level at runtime. A no-op before [`init`] has run.
pub fn set_level(level: LogLevel) {
    if let Some(handle) = RELOAD.get() {
        let _ = handle.reload(EnvFilter::new(level.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_levels_case_insensitively() {
        assert_eq!("error".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert_eq!("WARN".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("Info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("trace".parse::<LogLevel>().unwrap(), LogLevel::Trace);
    }

    #[test]
    fn rejects_unknown_levels() {
        assert!("verbose".parse::<LogLevel>().is_err());
    }

    #[test]
    fn round_trips_through_display() {
        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            assert_eq!(level.as_str().parse::<LogLevel>().unwrap(), level);
        }
    }
}

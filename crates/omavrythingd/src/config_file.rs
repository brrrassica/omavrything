//! Loading and validation of the daemon configuration file.
//!
//! The system service reads `/etc/omavrything/config.toml` (overridable with
//! `--config`). Every key is optional: a missing key falls back to the built-in
//! default, and a command-line flag overrides the file. Unknown keys are
//! rejected so a typo is reported rather than silently ignored.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::logging::LogLevel;

/// The default system-wide configuration path.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/omavrything/config.toml";

/// The on-disk configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Explicit roots to index. Empty means "discover from the mount table".
    pub roots: Option<Vec<PathBuf>>,
    /// Discover roots from the mount table instead of using `roots`.
    pub auto: Option<bool>,
    /// Exclude globs applied during traversal.
    pub excludes: Option<Vec<String>>,
    /// Unix socket path.
    pub socket: Option<PathBuf>,
    /// Snapshot path.
    pub snapshot: Option<PathBuf>,
    /// Disable snapshot persistence.
    pub no_snapshot: Option<bool>,
    /// Idle-gated rescan interval, in seconds (0 disables).
    pub scan_interval: Option<u64>,
    /// Snapshot write interval, in seconds (0 disables).
    pub snapshot_interval: Option<u64>,
    /// Watch roots with inotify.
    pub watch: Option<bool>,
    /// Serve the D-Bus control interface.
    pub dbus: Option<bool>,
    /// Detect and index NTFS partitions.
    pub ntfs: Option<bool>,
    /// Collect per-file size/mtime.
    pub metadata: Option<bool>,
    /// Log level: `error`, `warn`, `info`, `debug`, or `trace`.
    pub log_level: Option<String>,
}

impl FileConfig {
    /// Read, parse, and validate a configuration file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let config: FileConfig = toml::from_str(&text)
            .map_err(|e| format!("invalid TOML in {}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration, returning a human-readable error.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(roots) = &self.roots {
            for root in roots {
                if !root.is_absolute() {
                    return Err(format!("root is not absolute: {}", root.display()));
                }
            }
        }
        if let Some(socket) = &self.socket {
            if !socket.is_absolute() {
                return Err(format!("socket is not absolute: {}", socket.display()));
            }
        }
        if let Some(snapshot) = &self.snapshot {
            if !snapshot.is_absolute() {
                return Err(format!("snapshot is not absolute: {}", snapshot.display()));
            }
        }
        if let Some(excludes) = &self.excludes {
            for glob in excludes {
                if glob.trim().is_empty() {
                    return Err("exclude glob is empty".into());
                }
            }
        }
        if let Some(level) = &self.log_level {
            level.parse::<LogLevel>()?;
        }
        Ok(())
    }

    /// The configured log level, if present and valid.
    pub fn log_level(&self) -> Option<LogLevel> {
        self.log_level.as_ref().and_then(|s| s.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omavrything-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_a_full_config() {
        let dir = temp_dir("full");
        let path = write(
            &dir,
            "config.toml",
            r#"
roots = ["/home", "/mnt/data"]
excludes = ["**/.git/**", "**/node_modules/**"]
socket = "/run/omavrything/omavrything.sock"
snapshot = "/var/lib/omavrything/index.snapshot"
scan_interval = 1800
snapshot_interval = 600
watch = true
dbus = true
ntfs = true
metadata = true
log_level = "debug"
"#,
        );
        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.roots.as_ref().unwrap().len(), 2);
        assert_eq!(config.scan_interval, Some(1800));
        assert_eq!(config.log_level(), Some(LogLevel::Debug));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_config_is_valid() {
        let dir = temp_dir("empty");
        let path = write(&dir, "config.toml", "");
        let config = FileConfig::load(&path).unwrap();
        assert!(config.roots.is_none());
        assert!(config.log_level().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unknown_keys() {
        let dir = temp_dir("unknown");
        let path = write(&dir, "config.toml", "bogus = 1\n");
        let err = FileConfig::load(&path).unwrap_err();
        assert!(err.contains("invalid TOML"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_relative_paths() {
        let dir = temp_dir("relative");
        let path = write(&dir, "config.toml", "socket = \"relative.sock\"\n");
        let err = FileConfig::load(&path).unwrap_err();
        assert!(err.contains("not absolute"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_an_invalid_log_level() {
        let dir = temp_dir("level");
        let path = write(&dir, "config.toml", "log_level = \"verbose\"\n");
        let err = FileConfig::load(&path).unwrap_err();
        assert!(err.contains("invalid log level"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_an_empty_exclude() {
        let dir = temp_dir("exclude");
        let path = write(&dir, "config.toml", "excludes = [\"  \"]\n");
        let err = FileConfig::load(&path).unwrap_err();
        assert!(err.contains("exclude glob is empty"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_default_config_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/config/config.toml");
        let config = FileConfig::load(&path).expect("shipped config must be valid");
        assert_eq!(config.auto, Some(true));
    }
}

//! Loading, validation, and persistence of the daemon configuration file.
//!
//! The system service reads `/etc/omavrything/config.toml` (overridable with
//! `--config`). Every key is optional: a missing key falls back to the built-in
//! default, and a command-line flag overrides the file. Unknown keys are
//! rejected so a typo is reported rather than silently ignored.
//!
//! Runtime changes made through the D-Bus `SetConfig` method are persisted to a
//! separate **state** file (see `--state-config`), which is loaded over the base
//! file on the next start. Keeping the two apart means the shipped
//! `/etc/omavrything/config.toml` is never rewritten by the daemon.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::logging::LogLevel;

/// The default system-wide configuration path.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/omavrything/config.toml";

/// The on-disk configuration.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Explicit roots to index. Empty means "discover from the mount table".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<PathBuf>>,
    /// Discover roots from the mount table instead of using `roots`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// Exclude globs applied during traversal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excludes: Option<Vec<String>>,
    /// Unix socket path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Snapshot path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<PathBuf>,
    /// Disable snapshot persistence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_snapshot: Option<bool>,
    /// Idle-gated rescan interval, in seconds (0 disables).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_interval: Option<u64>,
    /// Snapshot write interval, in seconds (0 disables).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_interval: Option<u64>,
    /// Idle period (seconds) after which the index is evicted to its snapshot
    /// image (0 disables eviction).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evict_idle_secs: Option<u64>,
    /// Watch roots with inotify.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<bool>,
    /// Serve the D-Bus control interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbus: Option<bool>,
    /// Detect and index NTFS partitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ntfs: Option<bool>,
    /// Collect per-file size/mtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<bool>,
    /// Log level: `error`, `warn`, `info`, `debug`, or `trace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    /// Scratchpad tile opacity (0.0–1.0). A UI setting the daemon stores and
    /// serves but does not otherwise use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_opacity: Option<f64>,
    /// Build and consult the n-gram candidate index (F10). Disabling it bounds
    /// memory on very large indexes at the cost of a linear scan per query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_index: Option<bool>,
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
        if let Some(opacity) = self.background_opacity {
            if !(0.0..=1.0).contains(&opacity) {
                return Err(format!("background_opacity out of range: {opacity}"));
            }
        }
        Ok(())
    }

    /// The configured log level, if present and valid.
    pub fn log_level(&self) -> Option<LogLevel> {
        self.log_level.as_ref().and_then(|s| s.parse().ok())
    }

    /// Set one key from its string form, as received over D-Bus. Returns an
    /// error for an unknown key or an unparseable value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "excludes" => {
                self.excludes = Some(
                    value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "scan_interval" => {
                self.scan_interval = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid scan_interval: {value}"))?,
                );
            }
            "snapshot_interval" => {
                self.snapshot_interval = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid snapshot_interval: {value}"))?,
                );
            }
            "evict_idle_secs" => {
                self.evict_idle_secs = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid evict_idle_secs: {value}"))?,
                );
            }
            "ntfs" => {
                self.ntfs = Some(parse_bool(value)?);
            }
            "log_level" => {
                value.parse::<LogLevel>()?;
                self.log_level = Some(value.to_string());
            }
            "background_opacity" => {
                let opacity: f64 = value
                    .parse()
                    .map_err(|_| format!("invalid background_opacity: {value}"))?;
                if !(0.0..=1.0).contains(&opacity) {
                    return Err(format!("background_opacity out of range: {opacity}"));
                }
                self.background_opacity = Some(opacity);
            }
            "candidate_index" => {
                self.candidate_index = Some(parse_bool(value)?);
            }
            other => return Err(format!("unknown config key: {other}")),
        }
        Ok(())
    }

    /// Write the configuration to `path` atomically (temp file + `rename`).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        // An existing config keeps its mode; a new one is owner-only, since the
        // state config records the indexed roots and exclusions.
        let mode = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        // The temp file carries its final mode from the moment it exists, rather
        // than being written and tightened afterwards: another local process could
        // otherwise open it while it was still `0666 & ~umask` and keep read access
        // to the document through its descriptor. See `private_file`.
        {
            let mut file = omavrything_core::private_file::open_private(&tmp, mode)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            file.write_all(text.as_bytes())
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            file.sync_all()
                .map_err(|e| format!("cannot flush {}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }
}

/// Parse a boolean from the forms the UI and CLI send.
pub(crate) fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("invalid boolean: {other}")),
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
background_opacity = 0.6
candidate_index = false
"#,
        );
        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.roots.as_ref().unwrap().len(), 2);
        assert_eq!(config.scan_interval, Some(1800));
        assert_eq!(config.log_level(), Some(LogLevel::Debug));
        assert_eq!(config.background_opacity, Some(0.6));
        assert_eq!(config.candidate_index, Some(false));
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
    fn rejects_an_out_of_range_opacity() {
        let dir = temp_dir("opacity");
        let path = write(&dir, "config.toml", "background_opacity = 1.5\n");
        let err = FileConfig::load(&path).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_updates_fields_and_rejects_bad_values() {
        let mut config = FileConfig::default();
        config.set("ntfs", "false").unwrap();
        assert_eq!(config.ntfs, Some(false));
        config.set("log_level", "debug").unwrap();
        assert_eq!(config.log_level(), Some(LogLevel::Debug));
        config.set("snapshot_interval", "120").unwrap();
        assert_eq!(config.snapshot_interval, Some(120));
        config.set("background_opacity", "0.5").unwrap();
        assert_eq!(config.background_opacity, Some(0.5));

        assert!(config.set("bogus", "x").is_err());
        assert!(config.set("ntfs", "maybe").is_err());
        assert!(config.set("background_opacity", "2").is_err());
    }

    #[test]
    fn save_round_trips_through_load() {
        let dir = temp_dir("save");
        let path = dir.join("state.toml");
        let mut config = FileConfig::default();
        config.set("ntfs", "true").unwrap();
        config.set("log_level", "warn").unwrap();
        config.set("background_opacity", "0.4").unwrap();
        config.save(&path).unwrap();

        let reloaded = FileConfig::load(&path).unwrap();
        assert_eq!(reloaded.ntfs, Some(true));
        assert_eq!(reloaded.log_level(), Some(LogLevel::Warn));
        assert_eq!(reloaded.background_opacity, Some(0.4));
        // Unset keys are omitted, not written as null.
        assert!(reloaded.roots.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_state_config_is_owner_only_and_an_existing_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        let dir = temp_dir("save-mode");
        let path = dir.join("state.toml");
        let config = FileConfig::default();

        // A config that does not exist yet is created owner-only: it records the
        // indexed roots and exclusions.
        config.save(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600);

        // An existing file keeps the mode its owner chose, including a widened one.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        config.save(&path).unwrap();
        assert_eq!(mode_of(&path), 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_default_config_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/config/config.toml");
        let config = FileConfig::load(&path).expect("shipped config must be valid");
        assert_eq!(config.auto, Some(true));
    }
}

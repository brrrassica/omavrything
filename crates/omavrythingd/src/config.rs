//! Runtime configuration shared between the daemon threads and the D-Bus
//! control interface.
//!
//! Changes made through [`SharedConfig::set`] are applied to the running daemon
//! and persisted to the state config file, so they survive a restart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use omavrything_core::scan::ScanConfig;

use crate::config_file::{FileConfig, parse_bool};
use crate::logging::{self, LogLevel};

/// Everything needed to build a [`SharedConfig`].
pub struct ConfigInit {
    pub scan: ScanConfig,
    pub scan_interval: u64,
    pub snapshot_interval: u64,
    /// Idle period (seconds) after which the index is evicted (0 disables).
    pub evict_idle_secs: u64,
    pub snapshot: Option<PathBuf>,
    pub watch: bool,
    pub ntfs: bool,
    pub log_level: LogLevel,
    pub background_opacity: f64,
    /// Whether the n-gram candidate index is built (F10).
    pub candidate_index: bool,
    /// The persisted runtime overrides, updated on every `set`.
    pub state: FileConfig,
    /// Where to persist `state`; `None` disables persistence.
    pub state_path: Option<PathBuf>,
}

/// Runtime-tunable daemon configuration.
pub struct SharedConfig {
    scan: RwLock<ScanConfig>,
    scan_interval: AtomicU64,
    snapshot_interval: AtomicU64,
    /// Idle period (seconds) after which the index is evicted (0 disables).
    evict_idle_secs: AtomicU64,
    snapshot: Option<PathBuf>,
    watch: bool,
    /// Whether a scan/rescan is currently in progress.
    scanning: AtomicBool,
    ntfs: AtomicBool,
    log_level: RwLock<LogLevel>,
    /// `f64` bits, so the value can live in an atomic.
    background_opacity: AtomicU64,
    /// Whether the n-gram candidate index is built (F10).
    candidate_index: AtomicBool,
    state: RwLock<FileConfig>,
    state_path: Option<PathBuf>,
}

impl SharedConfig {
    pub fn new(init: ConfigInit) -> Self {
        Self {
            scan: RwLock::new(init.scan),
            scan_interval: AtomicU64::new(init.scan_interval),
            snapshot_interval: AtomicU64::new(init.snapshot_interval),
            evict_idle_secs: AtomicU64::new(init.evict_idle_secs),
            snapshot: init.snapshot,
            watch: init.watch,
            scanning: AtomicBool::new(false),
            ntfs: AtomicBool::new(init.ntfs),
            log_level: RwLock::new(init.log_level),
            background_opacity: AtomicU64::new(init.background_opacity.to_bits()),
            candidate_index: AtomicBool::new(init.candidate_index),
            state: RwLock::new(init.state),
            state_path: init.state_path,
        }
    }

    /// Whether a scan/rescan is currently in progress.
    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    /// Mark a scan as started or finished.
    pub fn set_scanning(&self, value: bool) {
        self.scanning.store(value, Ordering::Relaxed);
    }

    /// A snapshot of the current scan configuration.
    pub fn scan(&self) -> ScanConfig {
        self.scan.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn scan_interval(&self) -> u64 {
        self.scan_interval.load(Ordering::Relaxed)
    }

    pub fn snapshot_interval(&self) -> u64 {
        self.snapshot_interval.load(Ordering::Relaxed)
    }

    /// The idle period (seconds) after which the index is evicted (0 disables).
    pub fn evict_idle_secs(&self) -> u64 {
        self.evict_idle_secs.load(Ordering::Relaxed)
    }

    pub fn snapshot_path(&self) -> Option<PathBuf> {
        self.snapshot.clone()
    }

    /// Whether NTFS detection and indexing is enabled.
    pub fn ntfs(&self) -> bool {
        self.ntfs.load(Ordering::Relaxed)
    }

    /// The current log level.
    pub fn log_level(&self) -> LogLevel {
        *self.log_level.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The scratchpad tile opacity (0.0–1.0).
    pub fn background_opacity(&self) -> f64 {
        f64::from_bits(self.background_opacity.load(Ordering::Relaxed))
    }

    /// Whether the n-gram candidate index is built (F10).
    pub fn candidate_index(&self) -> bool {
        self.candidate_index.load(Ordering::Relaxed)
    }

    /// Apply a configuration change and persist it. Returns an error for
    /// unknown keys or invalid values.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "excludes" => {
                let mut scan = self.scan.write().unwrap_or_else(|e| e.into_inner());
                scan.excludes = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "scan_interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid scan_interval: {value}"))?;
                self.scan_interval.store(v, Ordering::Relaxed);
            }
            "snapshot_interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid snapshot_interval: {value}"))?;
                self.snapshot_interval.store(v, Ordering::Relaxed);
            }
            "evict_idle_secs" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid evict_idle_secs: {value}"))?;
                self.evict_idle_secs.store(v, Ordering::Relaxed);
            }
            "ntfs" => {
                let v = parse_bool(value)?;
                self.ntfs.store(v, Ordering::Relaxed);
            }
            "log_level" => {
                let level: LogLevel = value.parse()?;
                *self.log_level.write().unwrap_or_else(|e| e.into_inner()) = level;
                logging::set_level(level);
            }
            "background_opacity" => {
                let v: f64 = value
                    .parse()
                    .map_err(|_| format!("invalid background_opacity: {value}"))?;
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("background_opacity out of range: {v}"));
                }
                self.background_opacity
                    .store(v.to_bits(), Ordering::Relaxed);
            }
            "candidate_index" => {
                let v = parse_bool(value)?;
                self.candidate_index.store(v, Ordering::Relaxed);
            }
            other => return Err(format!("unknown config key: {other}")),
        }
        self.persist(key, value)
    }

    /// Record the change in the state config and write it to disk.
    fn persist(&self, key: &str, value: &str) -> Result<(), String> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.set(key, value)?;
        state.save(path)
    }

    /// The configuration as a string map, for `GetConfig`.
    pub fn as_map(&self) -> HashMap<String, String> {
        let scan = self.scan();
        let mut map = HashMap::new();
        map.insert(
            "roots".into(),
            scan.roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        map.insert("excludes".into(), scan.excludes.join(","));
        map.insert("scan_interval".into(), self.scan_interval().to_string());
        map.insert(
            "snapshot_interval".into(),
            self.snapshot_interval().to_string(),
        );
        map.insert(
            "evict_idle_secs".into(),
            self.evict_idle_secs().to_string(),
        );
        map.insert(
            "snapshot".into(),
            self.snapshot_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        map.insert("watch".into(), self.watch.to_string());
        map.insert("scanning".into(), self.scanning().to_string());
        map.insert("ntfs".into(), self.ntfs().to_string());
        map.insert("log_level".into(), self.log_level().to_string());
        map.insert(
            "background_opacity".into(),
            self.background_opacity().to_string(),
        );
        map.insert("candidate_index".into(), self.candidate_index().to_string());
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omavrything_core::scan::ScanConfig;

    fn cfg() -> SharedConfig {
        SharedConfig::new(ConfigInit {
            scan: ScanConfig::default(),
            scan_interval: 900,
            snapshot_interval: 300,
            evict_idle_secs: 120,
            snapshot: None,
            watch: true,
            ntfs: true,
            log_level: LogLevel::Info,
            background_opacity: 0.75,
            candidate_index: true,
            state: FileConfig::default(),
            state_path: None,
        })
    }

    #[test]
    fn set_updates_intervals_and_excludes() {
        let c = cfg();
        assert!(c.set("scan_interval", "60").is_ok());
        assert_eq!(c.scan_interval(), 60);
        assert!(c.set("snapshot_interval", "120").is_ok());
        assert_eq!(c.snapshot_interval(), 120);
        assert!(c.set("excludes", "**/.git/**, **/node_modules/**").is_ok());
        assert_eq!(c.scan().excludes, vec!["**/.git/**", "**/node_modules/**"]);
    }

    #[test]
    fn set_updates_ntfs_log_level_and_opacity() {
        let c = cfg();
        assert!(c.set("ntfs", "false").is_ok());
        assert!(!c.ntfs());
        assert!(c.set("log_level", "debug").is_ok());
        assert_eq!(c.log_level(), LogLevel::Debug);
        assert!(c.set("background_opacity", "0.5").is_ok());
        assert_eq!(c.background_opacity(), 0.5);
    }

    #[test]
    fn set_rejects_unknown_and_invalid() {
        let c = cfg();
        assert!(c.set("bogus", "x").is_err());
        assert!(c.set("scan_interval", "not-a-number").is_err());
        assert!(c.set("ntfs", "maybe").is_err());
        assert!(c.set("background_opacity", "2").is_err());
    }

    #[test]
    fn set_persists_to_the_state_file() {
        let dir = std::env::temp_dir().join(format!("omavrything-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");

        let c = SharedConfig::new(ConfigInit {
            scan: ScanConfig::default(),
            scan_interval: 900,
            snapshot_interval: 300,
            evict_idle_secs: 120,
            snapshot: None,
            watch: true,
            ntfs: true,
            log_level: LogLevel::Info,
            background_opacity: 0.75,
            candidate_index: true,
            state: FileConfig::default(),
            state_path: Some(path.clone()),
        });
        c.set("ntfs", "false").unwrap();
        c.set("background_opacity", "0.4").unwrap();

        let reloaded = FileConfig::load(&path).unwrap();
        assert_eq!(reloaded.ntfs, Some(false));
        assert_eq!(reloaded.background_opacity, Some(0.4));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn as_map_has_expected_keys() {
        let c = cfg();
        let map = c.as_map();
        for key in [
            "roots",
            "excludes",
            "scan_interval",
            "snapshot_interval",
            "snapshot",
            "watch",
            "scanning",
            "ntfs",
            "log_level",
            "background_opacity",
            "candidate_index",
        ] {
            assert!(map.contains_key(key), "missing {key}");
        }
    }

    #[test]
    fn scanning_flag_toggles_and_is_reported() {
        let c = cfg();
        assert!(!c.scanning());
        c.set_scanning(true);
        assert!(c.scanning());
        assert_eq!(c.as_map().get("scanning").map(String::as_str), Some("true"));
        c.set_scanning(false);
        assert!(!c.scanning());
    }
}

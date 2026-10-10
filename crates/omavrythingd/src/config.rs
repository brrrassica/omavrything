//! Runtime configuration shared between the daemon threads and the D-Bus
//! control interface.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use omavrything_core::scan::ScanConfig;

/// Runtime-tunable daemon configuration.
pub struct SharedConfig {
    scan: RwLock<ScanConfig>,
    scan_interval: AtomicU64,
    snapshot_interval: AtomicU64,
    snapshot: Option<PathBuf>,
    watch: bool,
}

impl SharedConfig {
    pub fn new(
        scan: ScanConfig,
        scan_interval: u64,
        snapshot_interval: u64,
        snapshot: Option<PathBuf>,
        watch: bool,
    ) -> Self {
        Self {
            scan: RwLock::new(scan),
            scan_interval: AtomicU64::new(scan_interval),
            snapshot_interval: AtomicU64::new(snapshot_interval),
            snapshot,
            watch,
        }
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

    pub fn snapshot_path(&self) -> Option<PathBuf> {
        self.snapshot.clone()
    }

    /// Apply a configuration change. Returns an error for unknown keys or
    /// invalid values.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "excludes" => {
                let mut scan = self.scan.write().unwrap_or_else(|e| e.into_inner());
                scan.excludes = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                Ok(())
            }
            "scan_interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid scan_interval: {value}"))?;
                self.scan_interval.store(v, Ordering::Relaxed);
                Ok(())
            }
            "snapshot_interval" => {
                let v: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid snapshot_interval: {value}"))?;
                self.snapshot_interval.store(v, Ordering::Relaxed);
                Ok(())
            }
            other => Err(format!("unknown config key: {other}")),
        }
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
            "snapshot".into(),
            self.snapshot_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        map.insert("watch".into(), self.watch.to_string());
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omavrything_core::scan::ScanConfig;

    fn cfg() -> SharedConfig {
        SharedConfig::new(ScanConfig::default(), 900, 300, None, true)
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
    fn set_rejects_unknown_and_invalid() {
        let c = cfg();
        assert!(c.set("bogus", "x").is_err());
        assert!(c.set("scan_interval", "not-a-number").is_err());
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
        ] {
            assert!(map.contains_key(key), "missing {key}");
        }
    }
}

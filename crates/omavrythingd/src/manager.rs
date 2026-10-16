//! The daemon's index manager: an owned index that can be evicted to a
//! memory-mapped snapshot image and reloaded on demand.
//!
//! While evicted, only the mapping and a resident path -> index map remain
//! resident; queries scan the image (merged with a small delta of live updates)
//! and the watcher records changes in the delta. The owned index is
//! materialized again when a mutation needs it, and evicted again when idle.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use memmap2::Mmap;
use omavrything_core::index::{DeltaIndex, Index, MemoryBreakdown, PathIndex, Root, SearchOutcome};
use omavrything_core::live::{self, FsEvent};
use omavrything_core::scan::ScanConfig;
use omavrything_core::snapshot::{self, IndexImage, RootIdentity};

/// Fold the delta into the owned index once it holds this many changes, so it
/// stays bounded under heavy filesystem churn.
const DELTA_FOLD_THRESHOLD: usize = 50_000;

/// The resident index state.
enum State {
    /// The owned index is resident and mutable. Boxed so the enum stays small
    /// (the `Image` variant is a mapping plus a map).
    Owned(Box<Index>),
    /// The index is evicted: only the mapping and the resident path map remain.
    Image {
        mmap: Arc<Mmap>,
        by_path: PathIndex,
    },
}

/// Owns the index and its eviction lifecycle.
pub struct IndexManager {
    state: RwLock<State>,
    delta: RwLock<DeltaIndex>,
    snapshot_path: Option<PathBuf>,
    identities: RwLock<Vec<RootIdentity>>,
    /// Millis since `start` of the last query or mutation, for idle eviction.
    last_activity: AtomicU64,
    /// Whether the initial index is still being built in the background. While
    /// set, searches return an empty page with the `loading` flag so clients
    /// can show a warming-up state instead of a silently empty index.
    loading: AtomicBool,
    start: Instant,
}

impl IndexManager {
    pub fn new(
        index: Index,
        identities: Vec<RootIdentity>,
        snapshot_path: Option<PathBuf>,
    ) -> Self {
        Self {
            state: RwLock::new(State::Owned(Box::new(index))),
            delta: RwLock::new(DeltaIndex::new()),
            snapshot_path,
            identities: RwLock::new(identities),
            last_activity: AtomicU64::new(0),
            loading: AtomicBool::new(false),
            start: Instant::now(),
        }
    }

    /// Set the initial-load flag.
    pub fn set_loading(&self, loading: bool) {
        self.loading.store(loading, Ordering::Relaxed);
    }

    /// Whether the initial index is still being built in the background.
    pub fn is_loading(&self) -> bool {
        self.loading.load(Ordering::Relaxed)
    }

    /// Swap a fully built index into the manager, replacing the (empty)
    /// warming-up index, and clear the loading flag.
    ///
    /// This is the hand-off point of the non-blocking startup: the socket and
    /// D-Bus serve an empty index while the scan runs; when it lands, every
    /// subsequent query sees the real index.
    pub fn replace_index(&self, index: Index, identities: Vec<RootIdentity>) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        *state = State::Owned(Box::new(index));
        *self.identities.write().unwrap_or_else(|e| e.into_inner()) = identities;
        self.loading.store(false, Ordering::Relaxed);
    }

    /// Record activity, resetting the idle-eviction timer.
    fn touch(&self) {
        self.last_activity
            .store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Run a query. Returns the outcome and whether the index is still loading.
    pub fn search(&self, query: &str, limit: usize, offset: usize) -> (SearchOutcome, bool) {
        self.touch();
        let loading = self.is_loading();
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => (index.search(query, limit, offset), loading),
            State::Image { mmap, .. } => {
                let Ok(image) = IndexImage::from_bytes(&mmap[..]) else {
                    return (SearchOutcome::default(), false);
                };
                let delta = self.delta.read().unwrap_or_else(|e| e.into_inner());
                let outcome = omavrything_core::index::search_image_with_delta(
                    image.records(),
                    image.paths(),
                    &delta,
                    query,
                    limit,
                    offset,
                );
                (outcome, loading)
            }
        }
    }

    /// Apply a filesystem event to the index (owned) or the delta (evicted).
    pub fn apply_event(&self, event: &FsEvent, config: &ScanConfig) -> bool {
        self.touch();
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let (changed, fold) = match &mut *state {
            State::Owned(index) => (live::apply(index, event, config), false),
            State::Image { mmap, by_path } => {
                let Ok(image) = IndexImage::from_bytes(&mmap[..]) else {
                    return false;
                };
                let changed = {
                    let mut delta = self.delta.write().unwrap_or_else(|e| e.into_inner());
                    live::apply_delta(
                        &mut delta,
                        by_path,
                        image.records(),
                        image.paths(),
                        event,
                        config,
                    )
                };
                let fold = self
                    .delta
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .len()
                    >= DELTA_FOLD_THRESHOLD;
                (changed, fold)
            }
        };
        // Fold the delta into the owned index when it grows past the threshold,
        // so it stays bounded under heavy churn. The idle timer evicts again.
        if fold {
            let owned = self.materialize(&state);
            *state = State::Owned(Box::new(owned));
        }
        changed
    }

    /// Run `f` with mutable access to the owned index, materializing it (and
    /// folding the delta) if the index is evicted.
    pub fn with_index_mut<R>(&self, f: impl FnOnce(&mut Index) -> R) -> R {
        self.touch();
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if matches!(&*state, State::Image { .. }) {
            let owned = self.materialize(&state);
            *state = State::Owned(Box::new(owned));
        }
        match &mut *state {
            State::Owned(index) => f(index),
            State::Image { .. } => unreachable!("materialized above"),
        }
    }

    /// Build an owned index from the evicted image, folding the delta in.
    fn materialize(&self, state: &State) -> Index {
        let State::Image { mmap, .. } = state else {
            unreachable!("materialize called on an owned index");
        };
        let image = IndexImage::from_bytes(&mmap[..]).expect("evicted image is valid");
        let mut index = image.to_index();
        let delta = self.delta.read().unwrap_or_else(|e| e.into_inner());
        index.apply_delta(&delta);
        drop(delta);
        self.delta
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        index
    }

    /// Evict the owned index to a memory-mapped snapshot. A no-op when already
    /// evicted or when there is no snapshot path.
    pub fn evict(&self) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let State::Owned(index) = &*state else {
            return;
        };
        let Some(path) = &self.snapshot_path else {
            return;
        };
        let ids = self
            .identities
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let bytes = snapshot::encode(index, &ids);
        if snapshot::write_atomic(&bytes, path).is_err() {
            return;
        }
        let Ok(file) = File::open(path) else {
            return;
        };
        // SAFETY: the snapshot is written atomically (temp file + rename), so a
        // mapped file is never mutated in place.
        let Ok(mmap) = (unsafe { Mmap::map(&file) }) else {
            return;
        };
        let Ok(image) = IndexImage::from_bytes(&mmap[..]) else {
            return;
        };
        let by_path = PathIndex::build(image.records(), image.paths());
        *state = State::Image {
            mmap: Arc::new(mmap),
            by_path,
        };
    }

    /// Evict the index if no query or mutation has occurred for `idle_secs`.
    /// Returns whether it evicted. A no-op when `idle_secs` is 0 or the index is
    /// already evicted.
    pub fn evict_if_idle(&self, idle_secs: u64) -> bool {
        if idle_secs == 0 || self.is_evicted() {
            return false;
        }
        let last = self.last_activity.load(Ordering::Relaxed);
        let now = self.start.elapsed().as_millis() as u64;
        if now.saturating_sub(last) < idle_secs.saturating_mul(1000) {
            return false;
        }
        self.evict();
        true
    }

    /// Reclaim slack in the owned index, if it is resident. A no-op when the
    /// index is evicted (the mapping has no reclaimable slack).
    pub fn shrink_if_owned(&self) -> bool {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        match &mut *state {
            State::Owned(index) => index.shrink_if_slack(),
            State::Image { .. } => false,
        }
    }

    /// Whether the delta holds any pending change.
    pub fn has_pending_delta(&self) -> bool {
        !self
            .delta
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Whether the index is currently evicted.
    pub fn is_evicted(&self) -> bool {
        matches!(
            &*self.state.read().unwrap_or_else(|e| e.into_inner()),
            State::Image { .. }
        )
    }

    /// The roots.
    pub fn roots(&self) -> Vec<Root> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.roots().to_vec(),
            State::Image { mmap, .. } => IndexImage::from_bytes(&mmap[..])
                .map(|image| image.roots().to_vec())
                .unwrap_or_default(),
        }
    }

    /// The number of live records.
    pub fn len(&self) -> usize {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.len(),
            State::Image { mmap, .. } => IndexImage::from_bytes(&mmap[..])
                .map(|image| image.len())
                .unwrap_or(0),
        }
    }

    /// An estimate of the resident bytes held by the index.
    pub fn memory_bytes(&self) -> u64 {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.memory_bytes(),
            State::Image { mmap, by_path } => {
                let delta = self.delta.read().unwrap_or_else(|e| e.into_inner());
                mmap.len() as u64 + by_path.memory_bytes() + delta.memory_bytes()
            }
        }
    }

    /// An estimate of the resident (capacity) bytes held by the index.
    pub fn capacity_bytes(&self) -> u64 {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.capacity_bytes(),
            State::Image { mmap, by_path } => mmap.len() as u64 + by_path.memory_bytes(),
        }
    }

    /// A per-component memory breakdown. Zeroed when the index is evicted (the
    /// owned arenas are not resident).
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.memory_breakdown(),
            State::Image { .. } => MemoryBreakdown::default(),
        }
    }

    /// Encode the current index (folding the delta) into a snapshot byte string.
    pub fn encode(&self) -> Vec<u8> {
        let ids = self
            .identities
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.with_index_mut(|index| snapshot::encode(index, &ids))
    }

    /// The root identities.
    pub fn identities(&self) -> Vec<RootIdentity> {
        self.identities
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omavrything_core::index::FsKind;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("omavrything-mgr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample() -> Index {
        let mut index = Index::new();
        index.add_root(Path::new("/"), FsKind::Other);
        index.insert(Path::new("/home/report.pdf"), false, 10, 1);
        index.insert(Path::new("/home/notes.md"), false, 20, 2);
        index
    }

    #[test]
    fn evict_then_search_uses_the_image() {
        let dir = temp_dir("evict");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap.clone()));

        assert!(!manager.is_evicted());
        manager.evict();
        assert!(manager.is_evicted());

        let (outcome, loading) = manager.search("report", 10, 0);
        assert!(!loading);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].path, "/home/report.pdf");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_index_mut_materializes_and_folds_the_delta() {
        let dir = temp_dir("materialize");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap.clone()));
        manager.evict();

        // A live update while evicted goes to the delta.
        let config = ScanConfig::with_roots(vec![PathBuf::from("/")]);
        manager.apply_event(&FsEvent::Removed(PathBuf::from("/home/notes.md")), &config);

        // A query sees the removal via the delta.
        let (outcome, _) = manager.search("notes", 10, 0);
        assert_eq!(outcome.total, 0);

        // Materializing folds the delta into the owned index.
        let len = manager.with_index_mut(|index| index.len());
        assert!(!manager.is_evicted());
        assert_eq!(len, 2); // root + report.pdf
        assert!(manager.with_index_mut(|index| index.find(Path::new("/home/notes.md"))).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

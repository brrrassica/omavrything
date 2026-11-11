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
use omavrything_core::snapshot::{self, RootIdentity, SectionMap};

use crate::SNAPSHOT_LOCK;

/// Fold the delta into the owned index once it holds this many changes, so it
/// stays bounded under heavy filesystem churn.
const DELTA_FOLD_THRESHOLD: usize = 50_000;

/// The resident index state.
enum State {
    /// The owned index is resident and mutable. Boxed so the enum stays small
    /// (the `Image` variant is a mapping plus a map).
    Owned(Box<Index>),
    /// The index is evicted: only the mapping and the resident path map remain.
    ///
    /// `sections` caches the section layout from the checksum-validated
    /// parse at eviction time. `IndexImage::from_bytes` verifies an FNV-1a
    /// checksum over the whole snapshot body, so re-parsing per call would
    /// let an event (or an unauthenticated local caller) force repeated
    /// O(snapshot) hashing; with the layout cached, `apply_event`'s evicted
    /// branch uses `image_from_sections`, which borrows the arenas from the
    /// mapping with no copy and no re-hash, and `GetStatus`/`ListRoots`
    /// read the `roots`/`len` caches and stay O(1). The mmap is never
    /// mutated in place, so the cached layout cannot go stale.
    Image {
        mmap: Arc<Mmap>,
        by_path: PathIndex,
        sections: SectionMap,
        roots: Vec<Root>,
        len: usize,
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
    /// Whether a background materialization of an evicted index is in flight,
    /// so concurrent queries start at most one.
    materializing: AtomicBool,
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
            materializing: AtomicBool::new(false),
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

    /// Record user activity, resetting the idle-eviction timer.
    ///
    /// Only user-driven work should call this: a query, or a mutation the user
    /// requested through the control interface. Background indexing, snapshot
    /// writes, and materialization must not, because they are not user
    /// activity and touching on them starves the evictor exactly like
    /// filesystem events did before rc8.
    pub fn touch(&self) {
        self.last_activity
            .store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Run a query. Returns the outcome and whether the index is still loading.
    ///
    /// When the index is evicted, this returns an empty page with `loading` set
    /// and does **not** scan the image: a linear scan of a multi-million-record
    /// image takes seconds (measured 8–16 s at 1.38 M), which is far too slow
    /// for an interactive query. The caller should start a background
    /// materialization with [`IndexManager::ensure_resident`]; the next query
    /// is then served from the owned index at full speed.
    pub fn search(&self, query: &str, limit: usize, offset: usize) -> (SearchOutcome, bool) {
        self.touch();
        let loading = self.is_loading();
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => (index.search(query, limit, offset), loading),
            State::Image { .. } => (SearchOutcome::default(), true),
        }
    }

    /// If the index is evicted, start a background materialization and return
    /// `true` (the caller should report `loading`). Returns `false` when the
    /// index is resident; returns `true` when a materialization is already in
    /// flight, so concurrent queries start at most one.
    ///
    /// The materialization folds the delta in and swaps the owned index back
    /// in, so the next query is served at full speed. It runs on its own thread
    /// because it takes ~1–2 s at 1.38 M records; the query that triggered it
    /// returns immediately with `loading` instead of blocking.
    pub fn ensure_resident(self: &Arc<Self>) -> bool {
        if !self.is_evicted() {
            return false;
        }
        if self.materializing.swap(true, Ordering::AcqRel) {
            return true;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("omavry-materialize".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                this.with_index_mut(|_| {});
                this.materializing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.materializing.store(false, Ordering::Release);
        }
        true
    }

    /// Apply a filesystem event to the index (owned) or the delta (evicted).
    ///
    /// Filesystem events never count as activity: "idle" means the user is
    /// not searching, not that the filesystem is quiet. A desktop fires
    /// inotify events (real changes, excluded paths, metadata noise) far more
    /// often than a typical idle period, and touching on any of them starves
    /// the idle evictor so the index never evicts and the resident set never
    /// falls. While evicted, events accumulate in the delta (folded
    /// back when it grows past [`DELTA_FOLD_THRESHOLD`]), so eviction under
    /// churn is safe.
    pub fn apply_event(&self, event: &FsEvent, config: &ScanConfig) -> bool {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let (changed, fold) = match &mut *state {
            State::Owned(index) => {
                let changed = live::apply(index, event, config);
                // The arenas are append-only, so sustained create/remove churn
                // leaves tombstones behind. Compact once they dominate, so the
                // owned index stays bounded even when snapshots are disabled
                // (the same retention bug the evicted delta had).
                index.compact_if_needed();
                (changed, false)
            }
            State::Image {
                mmap,
                by_path,
                sections,
                ..
            } => {
                // Reuse the layout validated at eviction: borrowing the arenas
                // from the mapping is O(1) and does not re-hash the snapshot per
                // event (issue 55).
                let Ok(view) = sections.view(&mmap[..]) else {
                    return false;
                };
                let changed = {
                    let mut delta = self.delta.write().unwrap_or_else(|e| e.into_inner());
                    live::apply_delta(
                        &mut delta,
                        by_path,
                        view.records(),
                        view.paths(),
                        event,
                        config,
                    )
                };
                let fold = self.delta.read().unwrap_or_else(|e| e.into_inner()).len()
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
    ///
    /// This does **not** reset the idle-eviction timer: it is used by
    /// background indexing, snapshot encoding, and materialization, none of
    /// which are user activity. User-driven callers must call [`touch`] at
    /// their entry point.
    pub fn with_index_mut<R>(&self, f: impl FnOnce(&mut Index) -> R) -> R {
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
        let State::Image { mmap, sections, .. } = state else {
            unreachable!("materialize called on an owned index");
        };
        // The checksum was verified when `sections` was built at eviction;
        // re-parse from the cached layout instead of re-hashing (issue 55).
        let image = sections.image(&mmap[..]).expect("evicted image is valid");
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

    /// Evict the owned index to a memory-mapped snapshot. Returns whether the
    /// index is now evicted. A no-op returning `false` when already evicted,
    /// when there is no snapshot path, or when the snapshot cannot be written.
    pub fn evict(&self) -> bool {
        // Acquire SNAPSHOT_LOCK before the state lock and hold it across the
        // encode, the write, and the mapping: `write_atomic` uses a fixed temp
        // path and requires concurrent writers to serialize, and ordering the
        // encode under the same lock prevents a periodic or shutdown save from
        // overwriting this newer snapshot on disk (issue 54). Every snapshot
        // writer follows this one lock order (snapshot lock before state lock)
        // so there is no deadlock cycle.
        let _snapshot_guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let State::Owned(index) = &*state else {
            return false;
        };
        let Some(path) = &self.snapshot_path else {
            return false;
        };
        let ids = self
            .identities
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let bytes = snapshot::encode(index, &ids);
        if snapshot::write_atomic(&bytes, path).is_err() {
            return false;
        }
        let Ok(file) = File::open(path) else {
            return false;
        };
        // SAFETY: the snapshot is written atomically (temp file + rename), so a
        // mapped file is never mutated in place.
        let Ok(mmap) = (unsafe { Mmap::map(&file) }) else {
            return false;
        };
        // Validate the mapping once: `sections_from_bytes` verifies the header
        // and the whole-snapshot checksum and returns the section layout;
        // `image` materializes the arenas and metadata from it without
        // re-hashing. The layout is cached in the state so per-event reads are
        // O(1) (issue 55).
        let Ok(sections) = snapshot::sections_from_bytes(&mmap[..]) else {
            return false;
        };
        let Ok(image) = sections.image(&mmap[..]) else {
            return false;
        };
        let by_path = PathIndex::build(image.records(), image.paths());
        // Cache the metadata the unauthenticated status methods need, so they
        // do not re-parse (and re-checksum) the mapping on every call.
        let roots = image.roots().to_vec();
        let len = image.len();
        *state = State::Image {
            mmap: Arc::new(mmap),
            by_path,
            sections,
            roots,
            len,
        };
        true
    }

    /// Seconds since the last query or mutation, capped at `cap`. Used by the
    /// evictor to sleep until the idle deadline instead of polling blindly.
    pub fn idle_secs(&self, cap: u64) -> u64 {
        let last = self.last_activity.load(Ordering::Relaxed);
        let now = self.start.elapsed().as_millis() as u64;
        now.saturating_sub(last).min(cap.saturating_mul(1000)) / 1000
    }

    /// Evict the index if no user activity has occurred for `idle_secs`.
    /// Returns whether the index is now evicted. A no-op returning `false` when
    /// `idle_secs` is 0, the index is already evicted, the idle deadline has
    /// not passed, or the snapshot cannot be written.
    pub fn evict_if_idle(&self, idle_secs: u64) -> bool {
        if idle_secs == 0 || self.is_evicted() {
            return false;
        }
        let last = self.last_activity.load(Ordering::Relaxed);
        let now = self.start.elapsed().as_millis() as u64;
        if now.saturating_sub(last) < idle_secs.saturating_mul(1000) {
            return false;
        }
        self.evict()
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

    /// Test-only view of the delta's retained size: `(records, memory_bytes)`.
    /// Retained records include tombstones, so this is what the retained-memory
    /// bound is asserted against.
    #[cfg(test)]
    pub fn delta_retained(&self) -> (usize, u64) {
        let delta = self.delta.read().unwrap_or_else(|e| e.into_inner());
        (delta.records().len(), delta.memory_bytes())
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
            // Cached at eviction time: no re-parse (and no O(snapshot) checksum)
            // on this unauthenticated path.
            State::Image { roots, .. } => roots.clone(),
        }
    }

    /// The number of live records.
    pub fn len(&self) -> usize {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.len(),
            // Cached at eviction time (see `roots`).
            State::Image { len, .. } => *len,
        }
    }

    /// An estimate of the resident bytes held by the index.
    pub fn memory_bytes(&self) -> u64 {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        match &*state {
            State::Owned(index) => index.memory_bytes(),
            State::Image { mmap, by_path, .. } => {
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
            State::Image { mmap, by_path, .. } => mmap.len() as u64 + by_path.memory_bytes(),
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
    fn evicted_roots_and_len_are_served_from_the_cache() {
        // `GetStatus` and `ListRoots` are callable by every local UID without
        // polkit. When evicted, they must read the metadata cached at eviction
        // time rather than re-parsing (and re-checksumming) the whole snapshot
        // on every call.
        let dir = temp_dir("evict-meta");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap));

        let roots_before = manager.roots();
        let len_before = manager.len();
        assert_eq!(len_before, 3); // root + report.pdf + notes.md

        manager.evict();
        assert!(manager.is_evicted());

        // The cached metadata matches the owned index exactly, and repeated
        // calls are stable.
        assert_eq!(manager.roots().len(), roots_before.len());
        assert_eq!(manager.roots()[0].path, roots_before[0].path);
        assert_eq!(manager.len(), len_before);
        assert_eq!(manager.len(), len_before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_then_search_reports_loading_and_materializes() {
        let dir = temp_dir("evict");
        let snap = dir.join("index.snapshot");
        let manager = Arc::new(IndexManager::new(sample(), Vec::new(), Some(snap.clone())));

        assert!(!manager.is_evicted());
        manager.evict();
        assert!(manager.is_evicted());

        // An evicted query reports `loading` and does not scan the image (a
        // linear scan of a multi-million-record image takes seconds).
        let (outcome, loading) = manager.search("report", 10, 0);
        assert!(loading);
        assert_eq!(outcome.hits.len(), 0);

        // `ensure_resident` starts a background materialization; wait for it.
        assert!(manager.ensure_resident());
        for _ in 0..200 {
            if !manager.is_evicted() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!manager.is_evicted(), "the index must be materialized");

        // The next query is served from the owned index at full speed.
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

        // An evicted query reports `loading`; the delta is folded on
        // materialization, not scanned here.
        let (outcome, loading) = manager.search("notes", 10, 0);
        assert!(loading);
        assert_eq!(outcome.total, 0);

        // Materializing folds the delta into the owned index.
        let len = manager.with_index_mut(|index| index.len());
        assert!(!manager.is_evicted());
        assert_eq!(len, 2); // root + report.pdf
        assert!(
            manager
                .with_index_mut(|index| index.find(Path::new("/home/notes.md")))
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_filesystem_event_never_resets_the_idle_timer() {
        // "Idle" means the user is not searching, not that the filesystem is
        // quiet: the desktop fires inotify events far more often than the
        // idle period, and touching on any of them starves the evictor
        // (twelfth E2E run).
        let dir = temp_dir("event-touch");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap));
        let config = ScanConfig::with_roots(vec![PathBuf::from("/")]);

        // An ignored event (the path does not exist).
        assert!(!manager.apply_event(
            &FsEvent::Modified(PathBuf::from("/home/does-not-exist")),
            &config
        ));

        // A real change.
        let file = dir.join("real.txt");
        std::fs::write(&file, b"contents").unwrap();
        assert!(manager.apply_event(&FsEvent::Created(file), &config));

        assert_eq!(
            manager.last_activity.load(Ordering::Relaxed),
            0,
            "a filesystem event must not count as activity"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_index_mut_does_not_reset_the_idle_timer() {
        // Background indexing, snapshot encoding, and materialization all go
        // through `with_index_mut`; none of them are user activity, so none of
        // them may starve the evictor.
        let dir = temp_dir("mut-touch");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap));

        manager.with_index_mut(|index| index.len());

        assert_eq!(
            manager.last_activity.load(Ordering::Relaxed),
            0,
            "a background mutation must not count as activity"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_reports_failure_when_the_snapshot_cannot_be_written() {
        let dir = temp_dir("evict-fail");
        // Make the snapshot's parent a regular file so `create_dir_all` fails.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let snap = blocker.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap));

        assert!(!manager.evict(), "a failed write must not report eviction");
        assert!(!manager.is_evicted());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_evicted_manager_bounds_delta_retention_under_create_remove_churn() {
        // The retained-memory finding: while the index is evicted, a processed
        // create followed by a deletion of an initially absent path nets zero
        // live changes but must not grow the delta without bound.
        let dir = temp_dir("delta-retention");
        let snap = dir.join("index.snapshot");
        let manager = Arc::new(IndexManager::new(sample(), Vec::new(), Some(snap)));
        manager.evict();
        assert!(manager.is_evicted(), "the manager must start evicted");

        let config = ScanConfig::with_roots(vec![PathBuf::from("/")]);
        let root = dir.join("churn");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("tmp-file-with-a-reasonably-long-name.txt");

        for _ in 0..300 {
            // Create the file so `symlink_metadata` succeeds, then process the
            // event and remove it again.
            std::fs::write(&file, b"x").unwrap();
            manager.apply_event(&FsEvent::Created(file.clone()), &config);
            std::fs::remove_file(&file).unwrap();
            manager.apply_event(&FsEvent::Removed(file.clone()), &config);

            // The logical delta stays empty after each complete cycle...
            assert!(
                !manager.has_pending_delta(),
                "a create/remove cycle must net zero live changes"
            );
            // ...and the retained records/bytes stay bounded regardless of how
            // many cycles have run.
            let (records, bytes) = manager.delta_retained();
            assert!(
                records <= 128,
                "retained delta records grew to {records} after churn"
            );
            assert!(bytes < 16 * 1024, "retained delta bytes grew to {bytes}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_evict_and_save_serialize_on_the_snapshot_lock() {
        // Issue 54: eviction's snapshot write and the periodic save share the
        // fixed `<snapshot>.tmp` path, so concurrent writers must serialize.
        // Without serialization one of the `write_atomic` calls fails on the
        // `O_EXCL` in `open_private`. Run evictors and savers together and
        // require every save to succeed and the final snapshot to decode.
        let dir = temp_dir("evict-save-race");
        let snap = dir.join("index.snapshot");
        let manager = Arc::new(IndexManager::new(sample(), Vec::new(), Some(snap.clone())));

        let mut handles = Vec::new();
        for i in 0..4 {
            let m = Arc::clone(&manager);
            let path = snap.clone();
            handles.push(std::thread::spawn(move || {
                if i % 2 == 0 {
                    // A saver: the `run_snapshot` pattern (lock, encode, write).
                    let mut failures = 0usize;
                    for _ in 0..30 {
                        let _guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                        let ids = m.identities();
                        let bytes = m.with_index_mut(|idx| snapshot::encode(idx, &ids));
                        if snapshot::write_atomic(&bytes, &path).is_err() {
                            failures += 1;
                        }
                    }
                    failures
                } else {
                    // An evictor: materialize (as a query would), then evict.
                    for _ in 0..30 {
                        m.with_index_mut(|_| {});
                        let _ = m.evict();
                    }
                    0
                }
            }));
        }
        let failures: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(
            failures, 0,
            "a serialized snapshot writer must never fail on the shared temp path"
        );

        // The on-disk snapshot is a complete, valid image either way.
        assert!(
            snapshot::load(&snap).is_ok(),
            "the final snapshot must decode"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evict_if_idle_returns_the_real_eviction_result() {
        let dir = temp_dir("evict-result");
        let snap = dir.join("index.snapshot");
        let manager = IndexManager::new(sample(), Vec::new(), Some(snap));

        // The manager starts with `last_activity = 0`; wait past the deadline.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(manager.evict_if_idle(1));
        assert!(manager.is_evicted());

        // A second call is a no-op: the index is already evicted.
        assert!(!manager.evict_if_idle(1));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

# Lock-Across-I/O Analysis: Issues #117, #122, #123

## Root-Cause Class
All three issues share the same pattern: a **state write lock held across encode, then released before disk I/O** (write_atomic/fsync/rename). The disk I/O runs without the lock, relying on a separate serialization mechanism (SNAPSHOT_LOCK or mutation counters) to preserve ordering.

---

## Issue #117: `evict()` + `run_snapshot()` — SNAPSHOT_LOCK + state.write() across encode + write_atomic

### `evict()` (crates/omavrythingd/src/manager.rs:305-399)

| Line | Action |
|------|--------|
| 319 | `let _snapshot_guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());` — acquires **SNAPSHOT_LOCK** |
| 323 | `let state = self.state.write().unwrap_or_else(|e| e.into_inner());` — acquires **state.write()** |
| 334 | `let bytes = snapshot::encode(index, &ids);` — **encode under BOTH locks** |
| 340 | `drop(state);` — **state.write() dropped before I/O** |
| 341 | `snapshot::write_atomic(&bytes, path)` — **write_atomic without state lock, but SNAPSHOT_LOCK still held** |

**Verdict:** Holds `SNAPSHOT_LOCK + state.write()` across `encode`, releases `state.write()` before `write_atomic`, but **keeps `SNAPSHOT_LOCK` across the entire I/O sequence** (fsync, rename, mmap, checksum validation). The `mutations` counter (line 339) guards against stale encodes.

### `run_snapshot()` (crates/omavrythingd/src/main.rs:1047-1081)

| Line | Action |
|------|--------|
| 1067 | `let _guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());` — acquires **SNAPSHOT_LOCK** |
| 1069-1072 | `index.with_index_mut(|idx| { idx.compact(); snapshot::encode(idx, &ids) });` — **encode under both locks** (with_index_mut holds state.write) |
| 1073 | `drop(_guard);` — **SNAPSHOT_LOCK dropped BEFORE write_atomic** |
| 1074 | `snapshot::write_atomic(&bytes, &path)` — **write_atomic without SNAPSHOT_LOCK, state.write already dropped** |

**Verdict:** Holds `SNAPSHOT_LOCK + state.write()` across `encode`, then **releases both** before `write_atomic`. Relies purely on SNAPSHOT_LOCK serialization between concurrent invocations (no per-write generation check).

### Shutdown thread (crates/omavrythingd/src/main.rs:1155-1163)

| Line | Action |
|------|--------|
| 1158 | `let _guard = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());` — acquires **SNAPSHOT_LOCK** |
| 1159 | `let bytes = index.encode();` → `with_index_mut` → **encode under state.write()** |
| 1160 | `snapshot::write_atomic(&bytes, path)` — **write_atomic with SNAPSHOT_LOCK still held, state.write already dropped** |

**Verdict:** Holds `SNAPSHOT_LOCK` across `encode + write_atomic`; `state.write()` released by `with_index_mut` before `write_atomic`.

### Issue #122 Comment (lines 1155-1157)
> "Encode and write under SNAPSHOT_LOCK (same order as `evict()` and `run_snapshot`) so a concurrent eviction cannot interleave and persist an older image (issue 54)."

**Reality check:** `run_snapshot()` actually **drops** `SNAPSHOT_LOCK` before `write_atomic` (line 1073), contradicting the comment. The shutdown thread matches `evict()` in holding the lock across I/O.

---

## Issue #123: `remember_volume`/`forget_volume`/`persist` — state write lock across `FileConfig::save`

### `remember_volume` (crates/omavrythingd/src/config.rs:232-242)

| Line | Action |
|------|--------|
| 236 | `let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());` — **acquires state.write lock** |
| 241 | `state.save(path)` — **calls save with state.write held** |

`FileConfig::save` (config_file.rs:239-264) performs: `create_dir_all` → `temp file write` → `sync_all` → `rename`.

### `forget_volume` (crates/omavrythingd/src/config.rs:245-254)

| Line | Action |
|------|--------|
| 249 | `let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());` — **acquires state.write lock** |
| 253 | `state.save(path)` — **calls save with state.write held** |

### `persist` (crates/omavrythingd/src/config.rs:326-333)

| Line | Action |
|------|--------|
| 330 | `let mut state = self.state.write().unwrap_or_else(|e| e.into_inner);` — **acquires state.write lock** |
| 331 | `state.set(key, value)?;` — modifies state |
| 332 | `state.save(path)` — **calls save with state.write held** |

### `remembered_volumes` readers (config.rs:222-229)

| Line | Action |
|------|--------|
| 223-225 | `self.state.read().unwrap_or_else(|e| e.into_inner()).volumes.clone()` — **acquires state READ lock** |

**Verdict:** `remember_volume`, `forget_volume`, and `persist` all **hold `state.write()` across the full `FileConfig::save` sequence** (create_dir_all, temp write, sync_all, rename). `remembered_volumes` is a reader using `.read()`.

---

## (a) Is releasing state lock before disk I/O safe for snapshot ordering?

**Yes — an existing mechanism already handles this.**

- **`evict()`** uses a **mutation counter** (`self.mutations`, `AtomicU64`): after dropping the state lock and running `write_atomic`, it captures `generation = self.mutations.load(Ordering::Relaxed)`. On re-acquiring the state lock (line 376), it checks `self.mutations.load(Ordering::Relaxed) != generation`. If mutations occurred during I/O, the encoded bytes are stale and the eviction is discarded, retrying with fresh data.

- **`run_snapshot()`** and the **shutdown thread** rely on **SNAPSHOT_LOCK serialization**: only one writer active at a time on the fixed temp path, so concurrent writes cannot interleave. No per-write sequence check is needed because the lock enforces exclusivity.

- **Config state** (`remember_volume`/`forget_volume`/`persist`) has **no such mechanism**. The state write lock is held across disk I/O, which blocks all other readers/writers of `SharedConfig::state` (including `remembered_volumes` reads). A fix would need either:
  1. Release the state lock before `FileConfig::save` (safe since `save` is idempotent), or
  2. Add a mutation-generation check analogous to `IndexManager::evict`.

---

## (b) Rough blast radius: functions taking these locks

| Function | Lock held | Across I/O? | File:Line |
|----------|-----------|-------------|-----------|
| `IndexManager::evict` | `SNAPSHOT_LOCK + state.write()` | state.write released before write_atomic; SNAPSHOT_LOCK held throughout | manager.rs:319,323,340,341 |
| `run_snapshot` | `SNAPSHOT_LOCK + state.write()` (via with_index_mut) | **Both released** before write_atomic | main.rs:1067,1073 |
| Shutdown thread | `SNAPSHOT_LOCK` (state.write released by with_index_mut) | **SNAPSHOT_LOCK held** across write_atomic | main.rs:1158,1160 |
| `SharedConfig::remember_volume` | `state.write()` | **state.write held** across FileConfig::save | config.rs:236,241 |
| `SharedConfig::forget_volume` | `state.write()` | **state.write held** across FileConfig::save | config.rs:249,253 |
| `SharedConfig::persist` | `state.write()` | **state.write held** across FileConfig::save+set | config.rs:330,332 |
| `SharedConfig::remembered_volumes` | `state.read()` | No I/O | config.rs:223-225 |
| `IndexManager::search` | `state.read()` | No I/O | manager.rs:154 |
| `IndexManager::roots/len/memory_bytes` | `state.read()` | No I/O | manager.rs:456,463,474,484,504,519 |

**Summary count:** 3 functions hold state write lock across disk I/O (config issue #123); 3 functions touch SNAPSHOT_LOCK + state across encode+write (issues #117/#122), with different release strategies.
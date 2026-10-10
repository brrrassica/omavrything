//! Fuzz the snapshot loader.
//!
//! The loader parses an untrusted, on-disk binary format. It must never panic,
//! read out of bounds, or allocate unbounded memory on malformed input; it must
//! only ever return `Ok` or a `SnapshotError`. Any panic is a bug.
#![no_main]

use libfuzzer_sys::fuzz_target;
use omavrything_core::snapshot;

fuzz_target!(|data: &[u8]| {
    // The result is intentionally ignored: the contract is "no panic", not a
    // particular outcome. A successful decode is also re-encoded to exercise the
    // writer against attacker-influenced index contents.
    if let Ok((index, identities)) = snapshot::decode(data) {
        let _ = snapshot::encode(&index, &identities);
    }
});

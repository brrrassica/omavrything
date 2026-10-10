//! Fuzz the socket protocol request parser.
//!
//! `parse_request` is the single entry point for untrusted socket input. It must
//! never panic on arbitrary bytes; it must only return `Ok` or an error string.
#![no_main]

use libfuzzer_sys::fuzz_target;
use omavrything_core::protocol;

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = protocol::parse_request(data) {
        // Exercise the derived accessors so their arithmetic is covered too.
        let _ = request.effective_limit();
        let _ = request.effective_offset();
    }
});

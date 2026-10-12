//! Fuzz target: the count-only scan vs the one-shot and streaming decoders
//! (see `scan_check.rs`).

#![no_main]
use libfuzzer_sys::fuzz_target;

include!("scan_check.rs");

fuzz_target!(|data: &[u8]| check(data));

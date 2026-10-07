//! Fuzz target: zenflate one-shot vs streaming vs miniz_oxide on arbitrary
//! raw DEFLATE (see `inflate_diff_check.rs`). Catches wrong output, not just
//! crashes.

#![no_main]
use libfuzzer_sys::fuzz_target;

include!("inflate_diff_check.rs");

fuzz_target!(|data: &[u8]| check(data));

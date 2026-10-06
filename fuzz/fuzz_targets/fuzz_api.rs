//! Fuzz target: every compression entry point against two decoders.
//!
//! The fuzzer picks a level (`new(0..=31)`, `libdeflate(0..=12)`), an entry
//! point and its parameters, and the data. Data is used raw or expanded from
//! a small op stream into runs, repeats and literals, so the matchfinders and
//! block splitters see image-like structure. Checks:
//! - whole-buffer deflate/zlib/gzip into exactly-bound buffers decode back
//!   (zenflate and miniz_oxide);
//! - a too-small buffer returns `InsufficientSpace` or a valid stream;
//! - a `Stop` firing after N checks returns `Stopped` or the normal output;
//! - after either, the same compressor matches a fresh one byte for byte;
//! - incremental compression at fuzz-chosen cut points decodes back, and
//!   unsupported levels return an error;
//! - parallel gzip at 1-8 threads decodes back;
//! - independent segments (PNG iDOT layout) at fuzz-chosen ends decode back
//!   as one zlib stream and do not depend on compressor reuse.

#![no_main]
use libfuzzer_sys::fuzz_target;

include!("api_check.rs");

fuzz_target!(|input: Input| check(&input));

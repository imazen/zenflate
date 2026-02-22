//! Fuzz target: throw arbitrary bytes at all decompression formats.
//!
//! Goal: no panics, no out-of-bounds, no infinite loops.
//! Invalid data should return Err, not crash.

#![no_main]
use libfuzzer_sys::fuzz_target;
use zenflate::{Decompressor, Unstoppable};

fuzz_target!(|data: &[u8]| {
    let mut d = Decompressor::new();
    // Output buffer large enough for small valid streams, small enough
    // to avoid OOM on garbage that claims huge decompressed sizes.
    let mut output = vec![0u8; 64 * 1024];

    // Raw DEFLATE — self-terminating, so any prefix is fair game
    let _ = d.deflate_decompress(data, &mut output, Unstoppable);

    // zlib — validates 2-byte header + Adler-32 footer
    let _ = d.zlib_decompress(data, &mut output, Unstoppable);

    // gzip — validates 10-byte header + CRC-32/ISIZE footer
    let _ = d.gzip_decompress(data, &mut output, Unstoppable);
});

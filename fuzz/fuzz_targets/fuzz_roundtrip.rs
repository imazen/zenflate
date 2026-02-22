//! Fuzz target: compress then decompress, verify byte-identical output.
//!
//! Tests all three formats (deflate, zlib, gzip) at a fuzz-selected level.
//! Catches compression bugs where output decompresses to different data.

#![no_main]
use libfuzzer_sys::fuzz_target;
use zenflate::{CompressionLevel, Compressor, Decompressor, Unstoppable};

#[derive(arbitrary::Arbitrary, Debug)]
struct Input {
    /// Compression level 0-12
    level: u8,
    /// Data to compress
    data: Vec<u8>,
}

fuzz_target!(|input: Input| {
    // Clamp level to valid range
    let level = (input.level % 13) as u32;
    let data = &input.data;

    // Skip very large inputs to avoid timeout
    if data.len() > 256 * 1024 {
        return;
    }

    let comp_level = CompressionLevel::new(level);
    let mut compressor = Compressor::new(comp_level);
    let mut decompressor = Decompressor::new();

    // --- DEFLATE ---
    let bound = Compressor::deflate_compress_bound(data.len());
    let mut compressed = vec![0u8; bound];
    let csize = compressor
        .deflate_compress(data, &mut compressed, Unstoppable)
        .expect("deflate compress should not fail with sufficient buffer");

    let mut output = vec![0u8; data.len()];
    let result = decompressor
        .deflate_decompress(&compressed[..csize], &mut output, Unstoppable)
        .expect("deflate decompress of our own output should not fail");
    assert_eq!(
        &output[..result.output_written],
        data,
        "deflate round-trip mismatch at L{level}"
    );

    // --- zlib ---
    let bound = Compressor::zlib_compress_bound(data.len());
    let mut compressed = vec![0u8; bound];
    let csize = compressor
        .zlib_compress(data, &mut compressed, Unstoppable)
        .expect("zlib compress should not fail with sufficient buffer");

    let mut output = vec![0u8; data.len()];
    let result = decompressor
        .zlib_decompress(&compressed[..csize], &mut output, Unstoppable)
        .expect("zlib decompress of our own output should not fail");
    assert_eq!(
        &output[..result.output_written],
        data,
        "zlib round-trip mismatch at L{level}"
    );

    // --- gzip ---
    let bound = Compressor::gzip_compress_bound(data.len());
    let mut compressed = vec![0u8; bound];
    let csize = compressor
        .gzip_compress(data, &mut compressed, Unstoppable)
        .expect("gzip compress should not fail with sufficient buffer");

    let mut output = vec![0u8; data.len()];
    let result = decompressor
        .gzip_decompress(&compressed[..csize], &mut output, Unstoppable)
        .expect("gzip decompress of our own output should not fail");
    assert_eq!(
        &output[..result.output_written],
        data,
        "gzip round-trip mismatch at L{level}"
    );
});

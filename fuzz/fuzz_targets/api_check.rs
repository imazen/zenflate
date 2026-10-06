// Checks behind the `fuzz_api` target, shared with the stable seed replay in
// `tests/fuzz_regression.rs` (both `include!` this file).

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use zenflate::{CompressionError, CompressionLevel, Compressor, Decompressor, Unstoppable};

#[derive(arbitrary::Arbitrary, Debug)]
pub struct Input {
    level: u8,
    mode: u8,
    expand: bool,
    stop_after: u8,
    cap: u16,
    threads: u8,
    cuts: Vec<u16>,
    data: Vec<u8>,
}

/// Op stream → image-like bytes: zero runs, literals, back-copies, byte runs.
fn expand(seed: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < seed.len() && out.len() < 300_000 {
        let op = seed[i] as usize;
        i += 1;
        match op >> 6 {
            0 => out.extend(std::iter::repeat_n(0u8, (op & 63) * 37 + 1)),
            1 => {
                let end = (i + (op & 63) + 1).min(seed.len());
                out.extend_from_slice(&seed[i..end]);
                i = end;
            }
            2 => {
                let d = ((op & 63) + 1) * 521 % 32768 + 1;
                let len = 3 + (op & 15) * 17;
                if out.len() >= d {
                    for _ in 0..len {
                        out.push(out[out.len() - d]);
                    }
                }
            }
            _ => {
                let b = seed.get(i).copied().unwrap_or(0);
                i += 1;
                out.extend(std::iter::repeat_n(b, (op & 63) * 5 + 3));
            }
        }
    }
    out
}

struct StopAfter(AtomicUsize);

impl enough::Stop for StopAfter {
    fn check(&self) -> Result<(), enough::StopReason> {
        match self.0.load(Relaxed) {
            0 => Err(enough::StopReason::Cancelled),
            n => {
                self.0.store(n - 1, Relaxed);
                Ok(())
            }
        }
    }
}

fn decodes_to(deflate: &[u8], data: &[u8], what: &str) {
    let mut out = vec![0u8; data.len()];
    let r = Decompressor::new()
        .deflate_decompress(deflate, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{what}: zenflate decode: {e:?}"));
    assert!(
        r.output_written == data.len() && out == data,
        "{what}: zenflate content"
    );
    let mz = miniz_oxide::inflate::decompress_to_vec(deflate)
        .unwrap_or_else(|e| panic!("{what}: miniz decode: {e:?}"));
    assert!(mz == data, "{what}: miniz content");
}

fn zlib_decodes_to(zlib: &[u8], data: &[u8], what: &str) {
    let mut out = vec![0u8; data.len()];
    let r = Decompressor::new()
        .zlib_decompress(zlib, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{what}: zenflate zlib decode: {e:?}"));
    assert!(
        r.output_written == data.len() && out == data,
        "{what}: zlib content"
    );
}

fn fresh_zlib(level: CompressionLevel, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
    let n = Compressor::new(level)
        .zlib_compress(data, &mut out, Unstoppable)
        .expect("zlib into its bound");
    out.truncate(n);
    out
}

/// Run every check `input` selects; panics on a violation.
pub fn check(input: &Input) {
    // Selector bytes keep their meaning so committed seeds keep testing what
    // they were written for: new levels and entry points take reserved values.
    let (level, incremental_ok) = match input.level % 64 {
        e @ 0..=31 => (CompressionLevel::new(e as u32), (10..=22).contains(&e)),
        l @ 32..=44 => {
            let l = (l - 32) as u32;
            (CompressionLevel::libdeflate(l), (1..=9).contains(&l))
        }
        p @ 45..=62 => {
            // png(13..) uses new(13..)'s parsers.
            let p = (p - 45) as u32;
            (CompressionLevel::png(p), (13..=22).contains(&p))
        }
        // Reserved (63).
        r => {
            let e = (r % 32) as u32;
            (CompressionLevel::new(e), (10..=22).contains(&e))
        }
    };
    let data = if input.expand {
        expand(&input.data)
    } else {
        input.data.clone()
    };
    let max = if level.effort() >= 31 {
        16 << 10
    } else {
        256 << 10
    };
    if data.len() > max {
        return;
    }
    let name = format!("{level:?} len {}", data.len());

    match input.mode % 8 {
        0 => {
            let mut c = Compressor::new(level);
            let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
            let n = c
                .deflate_compress(&data, &mut out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: deflate into its bound: {e:?}"));
            decodes_to(&out[..n], &data, &name);
            let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len())];
            let n = c
                .gzip_compress(&data, &mut out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: gzip into its bound: {e:?}"));
            let mut back = vec![0u8; data.len()];
            let r = Decompressor::new()
                .gzip_decompress(&out[..n], &mut back, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: gzip decode: {e:?}"));
            assert!(
                r.output_written == data.len() && back == data,
                "{name}: gzip content"
            );
        }
        1 => {
            // Short buffer, then reuse.
            let fresh = fresh_zlib(level, &data);
            let mut c = Compressor::new(level);
            let mut small = vec![0u8; input.cap as usize % (fresh.len() + 1)];
            match c.zlib_compress(&data, &mut small, Unstoppable) {
                Ok(n) => zlib_decodes_to(&small[..n], &data, &name),
                Err(CompressionError::InsufficientSpace) => {}
                Err(e) => panic!("{name}: short buffer: {e:?}"),
            }
            let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            let n = c.zlib_compress(&data, &mut out, Unstoppable).unwrap();
            assert!(out[..n] == fresh[..], "{name}: reuse after a short buffer");
        }
        2 => {
            // Stop after N checks, then reuse.
            let fresh = fresh_zlib(level, &data);
            let mut c = Compressor::new(level);
            let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            let stop = StopAfter(AtomicUsize::new(input.stop_after as usize));
            match c.zlib_compress(&data, &mut out, &stop) {
                Ok(n) => assert!(
                    out[..n] == fresh[..],
                    "{name}: completed differently under a stop"
                ),
                Err(CompressionError::Stopped(_)) => {}
                Err(e) => panic!("{name}: stop: {e:?}"),
            }
            let n = c.zlib_compress(&data, &mut out, Unstoppable).unwrap();
            assert!(out[..n] == fresh[..], "{name}: reuse after a stop");
        }
        3 => {
            // Incremental at fuzz-chosen cut points.
            let mut c = Compressor::new(level);
            let mut stream = Vec::new();
            let mut end = 0usize;
            for &cut in input.cuts.iter().take(64) {
                if end >= data.len() {
                    break;
                }
                let prev = end;
                end = (end + 1 + cut as usize).min(data.len());
                let mut out = vec![0u8; Compressor::deflate_compress_bound(end - prev) + 64];
                match c.deflate_compress_incremental(&data[..end], &mut out, false, Unstoppable) {
                    Ok(n) => stream.extend_from_slice(&out[..n]),
                    Err(e) => {
                        assert!(!incremental_ok, "{name}: incremental at {end}: {e:?}");
                        return;
                    }
                }
            }
            let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len() - end) + 64];
            match c.deflate_compress_incremental(&data, &mut out, true, Unstoppable) {
                Ok(n) => {
                    stream.extend_from_slice(&out[..n]);
                    decodes_to(&stream, &data, &format!("{name} incremental"));
                }
                Err(e) => assert!(!incremental_ok, "{name}: incremental final: {e:?}"),
            }
        }
        4 => {
            // Parallel gzip.
            let threads = 1 + input.threads as usize % 8;
            let mut c = Compressor::new(level);
            let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len()) + 5 * threads];
            let n = c
                .gzip_compress_parallel(&data, &mut out, threads, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: parallel x{threads}: {e:?}"));
            let mut back = vec![0u8; data.len()];
            let r = Decompressor::new()
                .gzip_decompress(&out[..n], &mut back, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: parallel x{threads} decode: {e:?}"));
            assert!(
                r.output_written == data.len() && back == data,
                "{name}: parallel content"
            );
        }
        // Reserved (5..=7).
        _ => {}
    }
}

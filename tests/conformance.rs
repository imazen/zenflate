//! Cross-API conformance: every compression level through every compression
//! entry point, on inputs sized around zenflate's internal boundaries.
//!
//! Oracles:
//! - three independent decoders (zenflate, libdeflate, miniz_oxide) agree with
//!   the input;
//! - every output fits its documented bound (the output buffer is exactly the
//!   bound, so overrunning it fails with `InsufficientSpace`);
//! - a compressor reused across inputs of different sizes produces the same
//!   bytes as a fresh one;
//! - incremental compression decodes to the input under several chunkings,
//!   and unsupported levels return an error instead of panicking;
//! - parallel gzip decodes to the input for several thread counts.
//!
//! The default run keeps inputs small enough for debug builds. The full run
//! (`ZENFLATE_CONFORMANCE=full`, `just conformance-full`, CI job "Conformance
//! (full)") adds inputs up to 1 MiB and runs in release.
#![cfg(feature = "compress")]

use zenflate::png::{StripCompressor, StripDecoder};
use zenflate::{CompressionLevel, Compressor, Decompressor, Unstoppable};

fn full() -> bool {
    std::env::var("ZENFLATE_CONFORMANCE").is_ok_and(|v| v == "full")
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Filtered-scanline-like rows: small residuals, flat (zero) stretches, and
/// rows that repeat the row above with a few edits.
fn png_rows(width: usize, height: usize, bpp: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let row = width * bpp;
    let mut out = Vec::with_capacity(height * (row + 1));
    let mut prev: Vec<u8> = Vec::new();
    for y in 0..height {
        out.push((y % 5) as u8);
        if y % 7 == 3 && !prev.is_empty() {
            let mut cur = prev.clone();
            for _ in 0..4 {
                let i = (r.next() as usize) % row;
                cur[i] = r.next() as u8;
            }
            out.extend_from_slice(&cur);
            prev = cur;
            continue;
        }
        let mut cur = Vec::with_capacity(row);
        for x in 0..row {
            let flat = ((x / 48) + (y / 9)) % 3 == 0;
            let v = r.next();
            cur.push(if flat {
                0
            } else {
                ((v % 9) as u8).wrapping_sub(4)
            });
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    out
}

/// Zero runs of the given lengths between literal bytes.
fn runs(lengths: &[usize], seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut out = Vec::new();
    for &n in lengths {
        out.push((r.next() as u8) | 1);
        out.extend(std::iter::repeat_n(0u8, n));
    }
    out.push(7);
    out
}

/// Literal noise with zero runs placed to straddle the given offsets.
fn runs_across(len: usize, offsets: &[usize], run: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut out: Vec<u8> = (0..len).map(|_| (r.next() as u8) | 1).collect();
    for &o in offsets {
        let start = o.saturating_sub(run / 2);
        let end = (start + run).min(len);
        out[start..end].fill(0);
    }
    out
}

/// A random block followed by copies of it at fixed distances.
fn distances(dists: &[usize], seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut out = r.bytes(300);
    for &d in dists {
        // Pad with noise so the copy source sits exactly `d` back.
        while out.len() < d {
            out.push(r.next() as u8);
        }
        let start = out.len() - d;
        for i in 0..200 {
            let b = out[start + i];
            out.push(b);
        }
        out.extend(r.bytes(50));
    }
    out
}

/// Many short matches: 4-byte tokens from a small vocabulary.
fn short_matches(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let vocab: Vec<[u8; 4]> = (0..64).map(|_| (r.next() as u32).to_le_bytes()).collect();
    let mut out = Vec::with_capacity(len + 4);
    while out.len() < len {
        out.extend_from_slice(&vocab[(r.next() % 64) as usize]);
    }
    out.truncate(len);
    out
}

fn text(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "the", "deflate", "stream", "of", "a", "filtered", "row", "and", "window", "match",
        "length", "distance", "huffman", "block", "header", "literal", "zero", "run", "png",
        "image", "compress", "level", "effort", "\n",
    ];
    let mut r = Rng(seed | 1);
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        out.extend_from_slice(WORDS[(r.next() % 24) as usize].as_bytes());
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// Noise with every byte odd, then `k` isolated zeros: the coded size steps
/// across the stored size one zero at a time.
fn crossover(n: usize, k: usize) -> Vec<u8> {
    let mut r = Rng(n as u64 | 1);
    let mut out: Vec<u8> = (0..n).map(|_| (r.next() as u8) | 1).collect();
    for i in 0..k.min(n / 2) {
        out[2 * i + 1] = 0;
    }
    out
}

fn inputs() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = vec![("empty".into(), Vec::new())];
    let mut r = Rng(0x5eed);
    for n in [
        1usize, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 63, 64, 65, 255, 256, 257, 258, 259,
    ] {
        v.push((format!("noise-{n}"), r.bytes(n)));
        v.push((format!("zeros-{n}"), vec![0; n]));
        v.push((format!("const-{n}"), vec![0xA5; n]));
        v.push((
            format!("alt-{n}"),
            (0..n).map(|i| (i % 2) as u8 * 200).collect(),
        ));
    }
    for n in [4999usize, 5000, 5001] {
        v.push((format!("noise-{n}"), r.bytes(n)));
    }
    let ks: Vec<usize> = if full() {
        (0..300).collect()
    } else {
        (0..300).step_by(37).collect()
    };
    for k in ks {
        v.push((format!("crossover-600-k{k}"), crossover(600, k)));
    }
    v.push(("png-64x64x3".into(), png_rows(64, 64, 3, 1)));
    v.push(("png-128x128x1".into(), png_rows(128, 128, 1, 2)));
    let mut lens: Vec<usize> = (1..=12).collect();
    lens.extend([257, 258, 259, 260, 261, 262, 515, 516, 517, 518]);
    v.push(("runs-short".into(), runs(&lens, 3)));
    v.push(("runs-window".into(), runs(&[32767, 32768, 32769, 3], 4)));
    v.push((
        "distances".into(),
        distances(&[1, 2, 3, 4, 7, 8, 256, 4096, 32767, 32768, 32769], 5),
    ));
    v.push(("short-matches-60k".into(), short_matches(60_000, 6)));
    v.push(("text-30k".into(), text(30_000, 7)));
    if full() {
        for n in [65535usize, 65536, 65537, 131_077] {
            v.push((format!("noise-{n}"), r.bytes(n)));
        }
        let ks: Vec<usize> = (0..2500).step_by(7).collect();
        for k in ks {
            v.push((format!("crossover-4999-k{k}"), crossover(4999, k)));
        }
        v.push(("png-256x256x3".into(), png_rows(256, 256, 3, 8)));
        v.push(("png-512x512x3".into(), png_rows(512, 512, 3, 9)));
        v.push(("png-1024x1024x1".into(), png_rows(1024, 1024, 1, 10)));
        let edges = [32768, 65536, 131_072, 262_144, 327_680];
        v.push((
            "runs-across-edges".into(),
            runs_across(400_000, &edges, 3000, 11),
        ));
        v.push(("runs-long".into(), runs(&[70_000, 300_000, 5], 12)));
        v.push(("short-matches-600k".into(), short_matches(600_000, 13)));
        v.push(("zeros-1m".into(), vec![0; 1 << 20]));
        v.push(("text-1m".into(), text(1 << 20, 14)));
    }
    v
}

#[derive(Clone, Copy)]
enum Family {
    New(u32),
    Libdeflate(u32),
    Png(u32),
}

fn levels() -> Vec<(String, Family, CompressionLevel)> {
    let mut v: Vec<(String, Family, CompressionLevel)> = (0..=31)
        .map(|e| {
            (
                format!("new({e})"),
                Family::New(e),
                CompressionLevel::new(e),
            )
        })
        .collect();
    v.extend((0..=12).map(|l| {
        (
            format!("libdeflate({l})"),
            Family::Libdeflate(l),
            CompressionLevel::libdeflate(l),
        )
    }));
    v.extend((0..=17).map(|e| {
        (
            format!("png({e})"),
            Family::Png(e),
            CompressionLevel::png(e),
        )
    }));
    v
}

/// FullOptimal runs at ~1 MB/s in release; keep it to small inputs.
fn too_slow(level: &CompressionLevel, len: usize) -> bool {
    level.effort() >= 31 && len > if full() { 64 << 10 } else { 2 << 10 }
}

fn decode_all(what: &str, deflate: &[u8], expected: &[u8]) {
    let mut out = vec![0u8; expected.len()];
    let r = Decompressor::new()
        .deflate_decompress(deflate, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{what}: zenflate decode: {e:?}"));
    assert_eq!(r.output_written, expected.len(), "{what}: zenflate length");
    assert!(out == expected, "{what}: zenflate content");
    let mut out = vec![0u8; expected.len()];
    let n = libdeflater::Decompressor::new()
        .deflate_decompress(deflate, &mut out)
        .unwrap_or_else(|e| panic!("{what}: libdeflate decode: {e:?}"));
    assert!(
        n == expected.len() && out == expected,
        "{what}: libdeflate content"
    );
    let mz = miniz_oxide::inflate::decompress_to_vec(deflate)
        .unwrap_or_else(|e| panic!("{what}: miniz decode: {e:?}"));
    assert!(mz == expected, "{what}: miniz content");
}

/// Whole-buffer deflate/zlib/gzip into exactly-bound buffers.
fn check_whole(name: &str, level: CompressionLevel, data: &[u8]) -> Vec<u8> {
    let mut c = Compressor::new(level);
    let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
    let n = c
        .deflate_compress(data, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{name}: deflate into its bound: {e:?}"));
    decode_all(&format!("{name} deflate"), &out[..n], data);
    let deflate = out[..n].to_vec();

    let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
    let n = c
        .zlib_compress(data, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{name}: zlib into its bound: {e:?}"));
    let mut back = vec![0u8; data.len()];
    let r = Decompressor::new()
        .zlib_decompress(&out[..n], &mut back, Unstoppable)
        .unwrap_or_else(|e| panic!("{name}: zlib decode: {e:?}"));
    assert!(
        r.output_written == data.len() && back == data,
        "{name}: zlib content"
    );
    let mz = miniz_oxide::inflate::decompress_to_vec_zlib(&out[..n])
        .unwrap_or_else(|e| panic!("{name}: miniz zlib decode: {e:?}"));
    assert!(mz == data, "{name}: miniz zlib content");

    let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len())];
    let n = c
        .gzip_compress(data, &mut out, Unstoppable)
        .unwrap_or_else(|e| panic!("{name}: gzip into its bound: {e:?}"));
    let mut back = vec![0u8; data.len()];
    let r = Decompressor::new()
        .gzip_decompress(&out[..n], &mut back, Unstoppable)
        .unwrap_or_else(|e| panic!("{name}: gzip decode: {e:?}"));
    assert!(
        r.output_written == data.len() && back == data,
        "{name}: gzip content"
    );
    let mut back = vec![0u8; data.len()];
    let m = libdeflater::Decompressor::new()
        .gzip_decompress(&out[..n], &mut back)
        .unwrap_or_else(|e| panic!("{name}: libdeflate gzip decode: {e:?}"));
    assert!(
        m == data.len() && back == data,
        "{name}: libdeflate gzip content"
    );
    deflate
}

fn incremental_supported(family: Family) -> bool {
    // HtGreedy (libdeflate(1)), Greedy, Lazy, Lazy2.
    match family {
        Family::Libdeflate(l) => (1..=9).contains(&l),
        Family::New(e) => (10..=22).contains(&e),
        // png(13..=22) use the same parsers as new(13..=22).
        Family::Png(e) => (13..=22).contains(&e),
    }
}

fn check_incremental(name: &str, level: CompressionLevel, data: &[u8]) {
    let mut r = Rng(data.len() as u64 ^ 0x1234_5678);
    let rows = 64 * 3 + 1;
    let random: Vec<usize> = (0..64).map(|_| 1 + (r.next() % 5000) as usize).collect();
    let schemes: [(&str, Vec<usize>); 5] = [
        ("one call", vec![data.len().max(1)]),
        ("step 7", vec![7]),
        ("step 1000", vec![1000]),
        ("row-sized", vec![rows]),
        ("random", random),
    ];
    for (scheme, steps) in schemes {
        if scheme == "step 7" && data.len() > 20_000 {
            continue;
        }
        let mut c = Compressor::new(level);
        let mut stream = Vec::new();
        let mut end = 0;
        let mut i = 0;
        while end < data.len() {
            let prev = end;
            end = (end + steps[i % steps.len()]).min(data.len());
            i += 1;
            let mut out = vec![0u8; Compressor::deflate_compress_bound(end - prev) + 64];
            let n = c
                .deflate_compress_incremental(&data[..end], &mut out, false, Unstoppable)
                .unwrap_or_else(|e| panic!("{name} incremental {scheme} at {end}: {e:?}"));
            stream.extend_from_slice(&out[..n]);
        }
        let mut out = vec![0u8; 64];
        let n = c
            .deflate_compress_incremental(data, &mut out, true, Unstoppable)
            .unwrap_or_else(|e| panic!("{name} incremental {scheme} final: {e:?}"));
        stream.extend_from_slice(&out[..n]);
        decode_all(&format!("{name} incremental {scheme}"), &stream, data);
    }
}

/// Independent segments (PNG iDOT layout), each into an exactly-bound
/// buffer with one reused compressor, framed as zlib: decodes as one stream,
/// and every segment decodes alone to its slice of the input.
fn check_segmented(name: &str, level: CompressionLevel, data: &[u8]) {
    let n = data.len();
    for seg_ends in [vec![n], vec![n / 3, 2 * n / 3, n]] {
        let mut c = StripCompressor::new(level);
        let mut z = c.zlib_header().to_vec();
        let mut cends = Vec::new();
        let mut start = 0;
        for (k, &end) in seg_ends.iter().enumerate() {
            let mut out = vec![0u8; StripCompressor::bound(end - start)];
            let len = c
                .compress(
                    &data[start..end],
                    k + 1 == seg_ends.len(),
                    &mut out,
                    Unstoppable,
                )
                .unwrap_or_else(|e| panic!("{name} segment {k} into its bound: {e:?}"));
            z.extend_from_slice(&out[..len]);
            cends.push(z.len());
            start = end;
        }
        z.extend_from_slice(&zenflate::adler32(1, data).to_be_bytes());
        *cends.last_mut().unwrap() = z.len();
        let mz = miniz_oxide::inflate::decompress_to_vec_zlib(&z)
            .unwrap_or_else(|e| panic!("{name} segmented: miniz decode: {e:?}"));
        assert!(mz == data, "{name} segmented: miniz content");
        // Each strip alone, with an empty window: a non-final strip plus an
        // empty final block (`03 00`) must decode in full, which holds only
        // if it ends byte-aligned on a block boundary with no final block.
        let mut prev = 2;
        for (k, (&cend, &dend)) in cends.iter().zip(&seg_ends).enumerate() {
            let last = k + 1 == seg_ends.len();
            let mut src = z[prev..if last { cend - 4 } else { cend }].to_vec();
            if !last {
                src.extend_from_slice(&[0x03, 0x00]);
            }
            let dstart = if k == 0 { 0 } else { seg_ends[k - 1] };
            let mut got = vec![0u8; dend - dstart];
            let r = Decompressor::new()
                .deflate_decompress(&src, &mut got, Unstoppable)
                .unwrap_or_else(|e| panic!("{name} segment {k} alone: {e:?}"));
            assert_eq!(
                (r.input_consumed, r.output_written),
                (src.len(), got.len()),
                "{name} segment {k} alone: framing"
            );
            assert!(
                got == data[dstart..dend],
                "{name} segment {k} alone: content"
            );
            prev = cend;
        }
        // The same strips through the public strip decoder, verified via the
        // combined Adler-32 against the trailer.
        let mut prev = 0;
        let mut adler = 1;
        let mut trailer = None;
        for (k, (&cend, &dend)) in cends.iter().zip(&seg_ends).enumerate() {
            let mut d = StripDecoder::new(&z[prev..cend], k == 0, 1 << 16);
            let mut got = Vec::new();
            while !d.is_done() {
                let o = d
                    .fill()
                    .unwrap_or_else(|e| panic!("{name} strip decoder {k}: {e:?}"));
                let m = o.len();
                got.extend_from_slice(o);
                d.advance(m);
            }
            let dstart = if k == 0 { 0 } else { seg_ends[k - 1] };
            assert!(
                got == data[dstart..dend],
                "{name} strip decoder {k}: content"
            );
            let last = k + 1 == seg_ends.len();
            assert_eq!(
                d.ended_at_strip_boundary(),
                !last,
                "{name} strip decoder {k}"
            );
            adler = zenflate::adler32_combine(adler, d.adler32(), got.len());
            trailer = d.trailer();
            prev = cend;
        }
        assert_eq!(trailer, Some(adler), "{name} strip decoder: trailer");
    }
}

#[cfg(feature = "threads")]
fn check_parallel(name: &str, level: CompressionLevel, data: &[u8]) {
    let threads: &[usize] = if full() { &[2, 3, 4, 7] } else { &[2, 4] };
    for &t in threads {
        let mut c = Compressor::new(level);
        let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len()) + 5 * t];
        let n = c
            .gzip_compress_parallel(data, &mut out, t, Unstoppable)
            .unwrap_or_else(|e| panic!("{name} parallel x{t}: {e:?}"));
        let mut back = vec![0u8; data.len()];
        let r = Decompressor::new()
            .gzip_decompress(&out[..n], &mut back, Unstoppable)
            .unwrap_or_else(|e| panic!("{name} parallel x{t} decode: {e:?}"));
        assert!(
            r.output_written == data.len() && back == data,
            "{name} parallel x{t}"
        );
    }
}

/// One test per level group so the harness runs them in parallel.
fn run_levels(filter: impl Fn(Family) -> bool) {
    let inputs = inputs();
    for (lname, family, level) in levels().into_iter().filter(|(_, f, _)| filter(*f)) {
        // One compressor reused across every input: its output must match a
        // fresh compressor's byte for byte.
        let mut reused = Compressor::new(level);
        for (iname, data) in &inputs {
            if too_slow(&level, data.len()) {
                continue;
            }
            let name = format!("{lname} {iname} ({} B)", data.len());
            let fresh = check_whole(&name, level, data);
            let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
            let n = reused
                .deflate_compress(data, &mut out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}: reused compressor: {e:?}"));
            assert!(
                out[..n] == fresh[..],
                "{name}: reused compressor output differs from fresh"
            );
            if incremental_supported(family) {
                check_incremental(&name, level, data);
            } else {
                let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
                let mut c = Compressor::new(level);
                // Unsupported levels must refuse, not panic or emit garbage.
                if let Ok(n) = c.deflate_compress_incremental(data, &mut out, true, Unstoppable) {
                    decode_all(
                        &format!("{name} incremental (unsupported level)"),
                        &out[..n],
                        data,
                    );
                }
            }
            #[cfg(feature = "threads")]
            check_parallel(&name, level, data);
            check_segmented(&name, level, data);
        }
    }
}

#[test]
fn conformance_store_turbo_fastht() {
    run_levels(|f| matches!(f, Family::New(0..=9)));
}

#[test]
fn conformance_greedy_lazy() {
    run_levels(|f| matches!(f, Family::New(10..=17)));
}

#[test]
fn conformance_lazy2() {
    run_levels(|f| matches!(f, Family::New(18..=22)));
}

#[test]
fn conformance_near_optimal() {
    run_levels(|f| matches!(f, Family::New(23..=30)));
}

#[test]
fn conformance_full_optimal() {
    run_levels(|f| matches!(f, Family::New(31..)));
}

#[test]
fn conformance_libdeflate_levels() {
    run_levels(|f| matches!(f, Family::Libdeflate(_)));
}

/// Stops after `n` successful checks.
struct StopAfter(std::sync::atomic::AtomicUsize, enough::StopReason);

impl StopAfter {
    fn new(n: usize, reason: enough::StopReason) -> Self {
        Self(n.into(), reason)
    }
}

impl enough::Stop for StopAfter {
    fn check(&self) -> Result<(), enough::StopReason> {
        use std::sync::atomic::Ordering::Relaxed;
        match self.0.load(Relaxed) {
            0 => Err(self.1),
            n => {
                self.0.store(n - 1, Relaxed);
                Ok(())
            }
        }
    }
}

fn recovery_inputs() -> Vec<(String, Vec<u8>)> {
    let mut v = vec![
        ("png-64x64x3".to_string(), png_rows(64, 64, 3, 21)),
        ("text-30k".to_string(), text(30_000, 22)),
        ("noise-5000".to_string(), Rng(23).bytes(5000)),
    ];
    if full() {
        v.push(("png-256x256x3".into(), png_rows(256, 256, 3, 24)));
        v.push(("short-matches-300k".into(), short_matches(300_000, 25)));
    }
    v
}

/// After a cancelled call or one that ran out of output space, the same
/// compressor must produce exactly what a fresh one does.
use enough::StopReason::{Cancelled, TimedOut};

fn check_recovery(filter: impl Fn(Family) -> bool) {
    for (lname, family, level) in levels().into_iter().filter(|(_, f, _)| filter(*f)) {
        for (iname, data) in recovery_inputs() {
            if too_slow(&level, data.len()) {
                continue;
            }
            let name = format!("{lname} {iname}");
            let mut fresh = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            let n = Compressor::new(level)
                .zlib_compress(&data, &mut fresh, Unstoppable)
                .unwrap();
            let fresh = &fresh[..n];
            let mut c = Compressor::new(level);
            let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            for (stop_after, reason) in [0usize, 1, 2, 5, 20]
                .into_iter()
                .flat_map(|n| [(n, Cancelled), (n, TimedOut)])
            {
                let stop = StopAfter::new(stop_after, reason);
                match c.zlib_compress(&data, &mut out, &stop) {
                    // A completed call must be the normal output, except that
                    // full-optimal parsing finishes a timed-out call with its
                    // best-so-far parse: any valid stream.
                    Ok(m) if reason == TimedOut && level.effort() >= 31 => {
                        let mut back = vec![0u8; data.len()];
                        let r = Decompressor::new()
                            .zlib_decompress(&out[..m], &mut back, Unstoppable)
                            .unwrap_or_else(|e| panic!("{name}: timed-out output: {e:?}"));
                        assert!(
                            r.output_written == data.len() && back == data,
                            "{name}: timed-out content"
                        );
                    }
                    Ok(m) => assert!(
                        out[..m] == *fresh,
                        "{name}: stop {stop_after} ({reason:?}) completed differently"
                    ),
                    Err(zenflate::CompressionError::Stopped(r)) => {
                        assert_eq!(r, reason, "{name}: stop reason")
                    }
                    Err(e) => panic!("{name}: stop {stop_after}: {e:?}"),
                }
                let m = c.zlib_compress(&data, &mut out, Unstoppable).unwrap();
                assert!(
                    out[..m] == *fresh,
                    "{name}: output after a stop at {stop_after} differs from fresh"
                );
            }
            for cap in [0usize, 1, 2, 7, n / 2, n.saturating_sub(1)] {
                let mut small = vec![0u8; cap];
                match c.zlib_compress(&data, &mut small, Unstoppable) {
                    Ok(m) => {
                        let mut back = vec![0u8; data.len()];
                        let r = Decompressor::new()
                            .zlib_decompress(&small[..m], &mut back, Unstoppable)
                            .unwrap_or_else(|e| panic!("{name}: fit in {cap} but decode: {e:?}"));
                        assert!(
                            r.output_written == data.len() && back == data,
                            "{name}: cap {cap}"
                        );
                    }
                    Err(zenflate::CompressionError::InsufficientSpace) => {}
                    Err(e) => panic!("{name}: cap {cap}: {e:?}"),
                }
                let m = c.zlib_compress(&data, &mut out, Unstoppable).unwrap();
                assert!(
                    out[..m] == *fresh,
                    "{name}: output after a {cap}-byte buffer differs from fresh"
                );
            }
            if incremental_supported(family) {
                check_fork(&name, level, &data);
            }
        }
    }
}

/// Incremental fork: clone (and snapshot/restore) halfway, continue both with
/// the same tail and with different tails.
fn check_fork(name: &str, level: CompressionLevel, data: &[u8]) {
    let half = data.len() / 2;
    let mut a = Compressor::new(level);
    let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len()) + 64];
    let n = a
        .deflate_compress_incremental(&data[..half], &mut out, false, Unstoppable)
        .unwrap();
    let head = out[..n].to_vec();
    let mut b = a.clone();
    let snap = a.snapshot();

    let finish = |c: &mut Compressor, all: &[u8]| -> Vec<u8> {
        let mut out = vec![0u8; Compressor::deflate_compress_bound(all.len()) + 64];
        let n = c
            .deflate_compress_incremental(all, &mut out, true, Unstoppable)
            .unwrap();
        out[..n].to_vec()
    };
    let tail_a = finish(&mut a, data);
    let tail_b = finish(&mut b, data);
    assert!(tail_a == tail_b, "{name}: clone continued differently");
    decode_all(
        &format!("{name} fork"),
        &[head.clone(), tail_a].concat(),
        data,
    );

    // A different tail from the snapshot of the same prefix.
    let mut other = data[..half].to_vec();
    other.extend(text(data.len() - half, 31));
    a.restore(snap);
    let tail = finish(&mut a, &other);
    decode_all(
        &format!("{name} fork other tail"),
        &[head, tail].concat(),
        &other,
    );
}

#[test]
fn recovery_new_levels() {
    check_recovery(|f| matches!(f, Family::New(_)));
}

#[test]
fn recovery_libdeflate_levels() {
    check_recovery(|f| matches!(f, Family::Libdeflate(_)));
}

#[test]
fn conformance_png_levels() {
    run_levels(|f| matches!(f, Family::Png(_)));
}

#[test]
fn recovery_png_levels() {
    check_recovery(|f| matches!(f, Family::Png(_)));
}

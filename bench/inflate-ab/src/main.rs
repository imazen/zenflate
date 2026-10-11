//! One-shot inflate, this checkout ("new") vs the pinned main ("old"): small /
//! medium / large streams of stored, fixed and dynamic blocks, and codec-corpus
//! PNG IDAT streams. Each group's baseline is "old"; the CI column is new vs old.

use zenbench::prelude::*;

fn noise(n: usize, k: u32) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x >> 24) as u32 % k) as u8
        })
        .collect()
}

/// Half text, half 16-symbol noise: matches and literals both.
fn mixed(n: usize) -> Vec<u8> {
    let text = b"the quick brown fox jumps over the lazy dog 0123456789; ";
    let mut v: Vec<u8> = text.iter().cycle().take(n / 2).copied().collect();
    v.extend(noise(n - n / 2, 16));
    v
}

fn zenflate_raw(data: &[u8], level: new::CompressionLevel) -> Vec<u8> {
    let mut c = new::Compressor::new(level);
    let mut out = vec![0u8; new::Compressor::deflate_compress_bound(data.len())];
    let n = c.deflate_compress(data, &mut out, new::Unstoppable).unwrap();
    out.truncate(n);
    out
}

/// Fixed-Huffman blocks only (miniz_oxide forced to static blocks).
fn fixed_raw(data: &[u8]) -> Vec<u8> {
    use miniz_oxide::deflate::core::{
        CompressorOxide, TDEFLFlush, compress, create_comp_flags_from_zip_params,
        deflate_flags::TDEFL_FORCE_ALL_STATIC_BLOCKS,
    };
    let flags = create_comp_flags_from_zip_params(6, -15, 0) | TDEFL_FORCE_ALL_STATIC_BLOCKS;
    let mut c = CompressorOxide::new(flags);
    let mut out = vec![0u8; data.len() * 2 + 1024];
    let (_, _, n) = compress(&mut c, data, &mut out, TDEFLFlush::Finish);
    out.truncate(n);
    out
}

/// Raw DEFLATE of the concatenated IDAT data (zlib header stripped).
fn idat_raw(png: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 8;
    let mut z = Vec::new();
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().ok()?) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            z.extend_from_slice(png.get(pos + 8..pos + 8 + len)?);
        }
        pos += 12 + len;
    }
    (z.len() > 6).then(|| z[2..].to_vec())
}

fn pngs(dir: &std::path::Path, out: &mut Vec<Vec<u8>>) {
    let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            pngs(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")) {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

/// A 64-byte-aligned output of `cap` bytes, so both sides write to equally aligned memory.
fn aligned(buf: &mut Vec<u8>, cap: usize) -> &mut [u8] {
    *buf = vec![0u8; cap + 64];
    let off = buf.as_ptr().align_offset(64);
    &mut buf[off..off + cap]
}

/// One group: decode every stream in `streams` per call, old vs new. Both sides read
/// the same input allocation. `INFLATE_AB_ONLY=<prefix>` runs only matching groups.
fn pair(suite: &mut Suite, name: &str, streams: Vec<Vec<u8>>) {
    if std::env::var("INFLATE_AB_ONLY").is_ok_and(|p| !name.starts_with(&p)) {
        return;
    }
    let mut outlen = 0;
    let mut cap = 0;
    for s in &streams {
        let mut big = vec![0u8; 256 << 20];
        let r = new::Decompressor::new()
            .deflate_decompress(s, &mut big, new::Unstoppable)
            .unwrap();
        outlen += r.output_written;
        cap = cap.max(r.output_written + 64);
    }
    let streams = std::sync::Arc::new(streams);
    suite.group(name, move |g| {
        g.throughput(Throughput::Bytes(outlen as u64));
        let s1 = streams.clone();
        g.bench("old", move |b| {
            let mut d = old::Decompressor::new();
            let mut buf = Vec::new();
            let out = aligned(&mut buf, cap);
            b.iter(|| {
                for s in s1.iter() {
                    black_box(d.deflate_decompress(s, out, old::Unstoppable).unwrap());
                }
            })
        });
        let s2 = streams.clone();
        g.bench("new", move |b| {
            let mut d = new::Decompressor::new();
            let mut buf = Vec::new();
            let out = aligned(&mut buf, cap);
            b.iter(|| {
                for s in s2.iter() {
                    black_box(d.deflate_decompress(s, out, new::Unstoppable).unwrap());
                }
            })
        });
        g.baseline("old");
    });
}

fn inflate_ab(suite: &mut Suite) {
    for (size, n) in [("small", 4 << 10), ("medium", 256 << 10), ("large", 16 << 20)] {
        let data = mixed(n);
        pair(suite, &format!("stored/{size}"), vec![zenflate_raw(&data, new::CompressionLevel::new(0))]);
        pair(suite, &format!("fixed/{size}"), vec![fixed_raw(&data)]);
        pair(suite, &format!("dynamic/{size}"), vec![zenflate_raw(&data, new::CompressionLevel::balanced())]);
    }
    let corpus = codec_corpus::Corpus::new().expect("codec-corpus cache");
    for set in ["pngsuite", "gb82"] {
        let mut files = Vec::new();
        pngs(&corpus.get(set).expect("corpus set"), &mut files);
        let streams: Vec<_> = files.iter().filter_map(|f| idat_raw(f)).collect();
        pair(suite, &format!("idat/{set} ({} streams)", streams.len()), streams);
    }
}

zenbench::main!(inflate_ab);

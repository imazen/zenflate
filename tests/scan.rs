//! The count-only scan against the decoders: zenflate's compressor at every level,
//! codec-corpus PNG IDAT streams, and truncated / corrupted copies of both
//! (`fuzz/fuzz_targets/scan_check.rs` holds the comparison).

#![cfg(feature = "compress")]

mod scan {
    #![allow(dead_code)]
    include!("../fuzz/fuzz_targets/scan_check.rs");
}

use std::path::Path;
use zenflate::{CompressionLevel, Compressor, Decompressor, Unstoppable, zlib_scan};

fn noise(n: usize, k: u32, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x >> 24) as u32 % k) as u8
        })
        .collect()
}

fn inputs() -> Vec<Vec<u8>> {
    let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog; "
        .iter()
        .cycle()
        .take(40_000)
        .copied()
        .collect();
    vec![
        Vec::new(),
        vec![42],
        vec![0; 70_000],
        text,
        noise(20_000, 256, 1),
        noise(50_000, 4, 2),
        noise(3_000, 16, 3),
    ]
}

/// Stop points spread over an output of `n` bytes, plus its edges.
fn stops(n: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (1..=8).map(|i| n * i / 8).collect();
    v.extend([1, 2, 3, n.saturating_sub(1)]);
    v
}

fn levels() -> Vec<CompressionLevel> {
    let mut v: Vec<_> = (0..=30).map(CompressionLevel::new).collect();
    v.extend((1..=18).map(CompressionLevel::png));
    v.extend((0..=12).map(CompressionLevel::libdeflate));
    v
}

/// Truncations and single-byte corruptions of `stream`.
fn damaged(stream: &[u8]) -> Vec<Vec<u8>> {
    let n = stream.len();
    let mut v = Vec::new();
    for cut in [
        0,
        1,
        2,
        n / 3,
        n / 2,
        n.saturating_sub(5),
        n.saturating_sub(1),
    ] {
        v.push(stream[..cut.min(n)].to_vec());
    }
    for (i, at) in [0, 1, n / 4, n / 2, n * 3 / 4, n.saturating_sub(1)]
        .into_iter()
        .enumerate()
    {
        if at < n {
            let mut d = stream.to_vec();
            d[at] ^= [0x01, 0x80, 0xff, 0x10, 0x5a, 0x03][i];
            v.push(d);
        }
    }
    v
}

#[test]
fn scan_matches_decode_at_every_level() {
    let (mut streams, mut damaged_n) = (0, 0);
    for data in inputs() {
        for level in levels() {
            let mut c = Compressor::new(level);
            let mut z = vec![0u8; Compressor::deflate_compress_bound(data.len())];
            let n = c.deflate_compress(&data, &mut z, Unstoppable).unwrap();
            z.truncate(n);
            scan::check_scan(&z, &stops(data.len()));
            z.extend_from_slice(b"\x00\xfftrailing");
            scan::check_scan(&z, &[]);
            streams += 2;
            if data.len() == 3_000 {
                for d in damaged(&z[..n]) {
                    scan::check_scan(&d, &stops(data.len()));
                    damaged_n += 1;
                }
            }
        }
    }
    eprintln!("scan vs decode: {streams} compressor streams, {damaged_n} damaged");
}

fn idat_stream(png: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 8;
    let mut z = Vec::new();
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().ok()?) as usize;
        let body = png.get(pos + 8..pos + 8 + len)?;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            z.extend_from_slice(body);
        }
        pos += 12 + len;
    }
    (z.len() > 2).then_some(z)
}

fn pngs(dir: &Path, out: &mut Vec<Vec<u8>>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            pngs(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")) {
            out.push(std::fs::read(&p).unwrap());
        }
    }
}

#[test]
fn scan_matches_decode_on_corpus_idat() {
    let corpus = codec_corpus::Corpus::new().expect("codec-corpus cache");
    let mut files = Vec::new();
    for set in ["pngsuite", "png-conformance"] {
        pngs(&corpus.get(set).expect("fetch corpus set"), &mut files);
    }
    let (mut streams, mut damaged_n) = (0, 0);
    let mut out = vec![0u8; 64 << 20];
    for png in &files {
        let Some(z) = idat_stream(png) else { continue };
        // zlib framing against zlib_decompress, then the raw stream through the shared check.
        let one = Decompressor::new()
            .with_skip_checksum(true)
            .zlib_decompress(&z, &mut out, Unstoppable);
        let s = zlib_scan(&z, None, Unstoppable);
        match (&one, &s) {
            (Ok(r), Ok(s)) => {
                assert_eq!(
                    (s.input_consumed, s.output_len),
                    (r.input_consumed, r.output_written)
                )
            }
            (Err(e), Err(s)) => assert_eq!(*e, s.error),
            _ => panic!("zlib scan {s:?} vs decode {one:?}"),
        }
        let raw = &z[2..];
        let total = s.map_or(0, |s| s.output_len);
        scan::check_scan(raw, &stops(total));
        streams += 1;
        if streams % 8 == 0 {
            for d in damaged(raw) {
                scan::check_scan(&d, &stops(total));
                damaged_n += 1;
            }
        }
    }
    assert!(streams > 150, "only {streams} corpus IDAT streams");
    eprintln!("scan vs decode: {streams} corpus IDAT streams, {damaged_n} damaged");
}

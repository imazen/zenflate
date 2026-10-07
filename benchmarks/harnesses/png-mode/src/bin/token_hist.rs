//! Per-stream token histograms for the ultra-table experiments
//! (`tables/train_tables.py`).
//!
//! For every 8-bit PNG under the given directory: apply the adaptive PNG
//! filter, tokenize like zenflate's ultra encoder (literals + zero runs found
//! on 8-byte chunks; runs of 6+ zeros become literal 0 + distance-1 matches),
//! and print one TSV row: path, channels, width, height, bytes, then three
//! 286-symbol histograms (whole stream, first 2 KiB, first 8 KiB).
#![allow(dead_code)] // helpers.rs is shared with the other binaries

use std::path::{Path, PathBuf};

include!("../helpers.rs");

const LENGTH_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163,
    195, 227, 258,
];

fn length_slot(len: usize) -> usize {
    let mut s = 0;
    for (i, &b) in LENGTH_BASE.iter().enumerate() {
        if b as usize <= len {
            s = i;
        }
    }
    s
}

fn for_each_match(mut m: usize, mut f: impl FnMut(usize)) {
    while m >= 261 {
        f(258);
        m -= 258;
    }
    if m > 258 {
        f(m - 3);
        m = 3;
    }
    f(m);
}

struct Counts([u64; 286]);

impl Counts {
    fn lit(&mut self, b: u8) {
        self.0[b as usize] += 1;
    }
    fn zeros(&mut self, n: usize) {
        if n < 6 {
            self.0[0] += n as u64;
            return;
        }
        self.0[0] += 1;
        for_each_match(n - 1, |m| {
            if m >= 3 {
                self.0[257 + length_slot(m)] += 1;
            } else {
                self.0[0] += m as u64;
            }
        });
    }
}

/// Port of zenflate's png_ultra::tokenize (same chunk/run logic).
fn tokenize(data: &[u8], c: &mut Counts) {
    let (chunks, tail) = data.as_chunks::<8>();
    let mut run = 0usize;
    for ch in chunks {
        let v = u64::from_le_bytes(*ch);
        if v == 0 {
            run += 8;
            continue;
        }
        let mut lo = 0;
        if run > 0 {
            lo = (v.trailing_zeros() / 8) as usize;
            c.zeros(run + lo);
            run = 0;
        }
        let hi = (v.leading_zeros() / 8) as usize;
        if lo == 0 && hi < 4 {
            for &b in ch {
                c.lit(b);
            }
            continue;
        }
        let end = if hi >= 4 { 8 - hi } else { 8 };
        for &b in &ch[lo..end] {
            c.lit(b);
        }
        if hi >= 4 {
            run = hi;
        }
    }
    for &b in tail {
        if b == 0 {
            run += 1;
        } else {
            if run > 0 {
                c.zeros(run);
                run = 0;
            }
            c.lit(b);
        }
    }
    if run > 0 {
        c.zeros(run);
    }
}

fn hist(data: &[u8]) -> [u64; 286] {
    let mut c = Counts([0; 286]);
    tokenize(data, &mut c);
    c.0[256] = 1; // end of block
    c.0
}

fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    let mut paths = Vec::new();
    collect_pngs(Path::new(&dir), &mut paths);
    paths.sort();
    let root = PathBuf::from(&dir);
    for p in &paths {
        let Some(img) = decode_png(p) else {
            eprintln!("skip {}", p.display());
            continue;
        };
        let data = t_png_filter(&img);
        let rel = p.strip_prefix(&root).unwrap_or(p).display().to_string();
        let mut line = format!("{rel}\t{}\t{}\t{}\t{}", img.ch, img.w, img.h, data.len());
        for part in [&data[..], &data[..data.len().min(2048)], &data[..data.len().min(8192)]] {
            for x in hist(part) {
                line.push('\t');
                line.push_str(&x.to_string());
            }
        }
        println!("{line}");
    }
}

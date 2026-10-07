//! Per-level setup cost: `Compressor::new` alone, and compressing a small
//! PNG-like input with a fresh compressor (what a caller that builds one per
//! call pays) versus a reused one.
//!
//! ```text
//! cargo run --release --example setup_cost                  # timing table
//! heaptrack cargo run --release --example setup_cost -- --one e19   # peak heap of one level
//! ```
//!
//! Level specs: `e<N>` = `CompressionLevel::new(N)`, `p<N>` = `CompressionLevel::png(N)`.
use std::hint::black_box;
use std::time::Instant;

use zenflate::{CompressionLevel, Compressor, Unstoppable};

fn level(spec: &str) -> CompressionLevel {
    let n: u32 = spec[1..].parse().expect("level number");
    match &spec[..1] {
        "e" => CompressionLevel::new(n),
        "p" => CompressionLevel::png(n),
        _ => panic!("level spec is e<N> or p<N>"),
    }
}

/// Filtered-scanline-like data: rows of small residuals with zero stretches.
fn png_like(width: usize, height: usize, bpp: usize, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let row = width * bpp;
    let mut out = Vec::with_capacity(height * (row + 1));
    for y in 0..height {
        out.push(if y % 3 == 0 { 1 } else { 4 });
        for x in 0..row {
            let flat = (x / 64 + y / 16) % 3 == 0;
            let r = next();
            out.push(if flat {
                0
            } else {
                ((r % 9) as u8).wrapping_sub(4)
            });
        }
    }
    out
}

fn median_us(mut f: impl FnMut(), min_reps: usize) -> f64 {
    let mut t = Vec::new();
    let start = Instant::now();
    while t.len() < min_reps || (start.elapsed().as_secs_f64() < 0.2 && t.len() < 2000) {
        let t0 = Instant::now();
        f();
        t.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    t.sort_by(f64::total_cmp);
    t[t.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "--one" {
        // One construction + one small compress, for heaptrack.
        let data = png_like(64, 64, 3, 7);
        let mut c = Compressor::new(level(&args[2]));
        let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
        let n = c.zlib_compress(&data, &mut out, Unstoppable).unwrap();
        eprintln!("{}: {} -> {n} bytes", args[2], data.len());
        return;
    }
    let specs = [
        "e0", "e1", "e5", "e8", "e10", "e13", "e15", "e17", "e19", "e22", "e24", "e26", "e30",
        "e31", "p1", "p2", "p3", "p6", "p12",
    ];
    let inputs = [
        ("24x24 gray", png_like(24, 24, 1, 3)),
        ("64x64 rgb", png_like(64, 64, 3, 5)),
        ("256x256 rgb", png_like(256, 256, 3, 9)),
    ];
    print!("{:6} {:>10}", "level", "new() us");
    for (name, _) in &inputs {
        print!(" | {:>22}", format!("{name}: fresh / reused us"));
    }
    println!();
    for spec in specs {
        let l = level(spec);
        let new_us = median_us(|| drop(black_box(Compressor::new(l))), 5);
        print!("{spec:6} {new_us:10.1}");
        for (_, data) in &inputs {
            let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            let reps = if spec == "e31" { 3 } else { 9 };
            let fresh = median_us(
                || {
                    let mut c = Compressor::new(l);
                    black_box(
                        c.zlib_compress(black_box(data), &mut out, Unstoppable)
                            .unwrap(),
                    );
                },
                reps,
            );
            let mut c = Compressor::new(l);
            let reused = median_us(
                || {
                    black_box(
                        c.zlib_compress(black_box(data), &mut out, Unstoppable)
                            .unwrap(),
                    );
                },
                reps,
            );
            print!(" | {fresh:10.1} / {reused:9.1}");
        }
        println!();
    }
}

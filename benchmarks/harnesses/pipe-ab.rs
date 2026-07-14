//! gzip compress+decompress pipeline A/B, avx512 on vs off.
//! Answers "does the avx512 checksum tier speed up the actual pipeline?"
use std::hint::black_box;
use std::time::Instant;
use zenflate::{Compressor, CompressionLevel, Decompressor, Unstoppable};

const TIER: &str = if cfg!(feature = "avx512") { "avx512" } else { "avx2" };

fn med_ms(samples: usize, mut f: impl FnMut()) -> f64 {
    for _ in 0..2 { f(); } // warmup
    let mut ts = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        f();
        ts.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ts.sort_by(f64::total_cmp);
    ts[samples / 2]
}

fn main() {
    let home = std::env::var("HOME").unwrap();
    let samples = 9;
    println!("# tier={TIER}  gzip effort=15 (balanced)  median-of-{samples}");
    println!("file,mib,comp_gibs,decomp_gibs");
    for name in ["xml", "nci", "x-ray"] {
        let data = std::fs::read(format!("{home}/.cache/compression-corpus/silesia/{name}")).unwrap();
        let n = data.len();
        // one gzip encode for the decode benchmark
        let mut comp = Compressor::new(CompressionLevel::balanced());
        let mut gz = vec![0u8; Compressor::gzip_compress_bound(n)];
        let gzlen = comp.gzip_compress(&data, &mut gz, Unstoppable).unwrap();
        let gz = &gz[..gzlen];

        let ct = med_ms(samples, || {
            let mut c = Compressor::new(CompressionLevel::balanced());
            let mut out = vec![0u8; Compressor::gzip_compress_bound(n)];
            black_box(c.gzip_compress(black_box(&data), &mut out, Unstoppable).unwrap());
        });
        let dt = med_ms(samples, || {
            let mut d = Decompressor::new();
            let mut out = vec![0u8; n];
            black_box(d.gzip_decompress(black_box(gz), &mut out, Unstoppable).unwrap());
        });
        let gibs = |ms: f64| (n as f64) / (ms / 1e3) / (1u64 << 30) as f64;
        println!("{name},{:.1},{:.3},{:.3}", n as f64 / 1e6, gibs(ct), gibs(dt));
    }
}

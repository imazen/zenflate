//! avx512-vs-avx2 A/B for zenflate checksums.
//! Built twice: `--features avx2` (v3/AVX2 max) and `--features avx512` (v4x).
//! Reports GiB/s (median of N) for adler32 + crc32 across cache→memory sizes.
use std::hint::black_box;
use std::time::Instant;

#[cfg(all(feature = "avx2", feature = "avx512"))]
compile_error!("pick exactly one of avx2 / avx512");

const TIER: &str = if cfg!(feature = "avx512") { "avx512(v4x)" } else { "avx2(v3)" };

fn fill(n: usize) -> Vec<u8> {
    // deterministic LCG — same bytes in both builds
    let mut s: u64 = 0x9e3779b97f4a7c15;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

fn med_gibs(buf: &[u8], per_sample_bytes: u64, samples: usize, mut f: impl FnMut(&[u8]) -> u32) -> f64 {
    let passes = (per_sample_bytes / buf.len() as u64).max(1);
    // warmup
    for _ in 0..passes.min(64) {
        black_box(f(black_box(buf)));
    }
    let mut ts = Vec::with_capacity(samples);
    for _ in 0..samples {
        let t = Instant::now();
        let mut acc = 0u32;
        for _ in 0..passes {
            acc ^= f(black_box(buf));
        }
        black_box(acc);
        ts.push(t.elapsed().as_secs_f64());
    }
    ts.sort_by(f64::total_cmp);
    let secs = ts[samples / 2];
    let bytes = passes * buf.len() as u64;
    (bytes as f64) / secs / (1u64 << 30) as f64
}

fn main() {
    let per_sample: u64 = 512 << 20; // ~512 MiB processed per timed sample
    let samples = 15;
    println!("# tier={TIER}  per_sample={}MiB  samples={samples}", per_sample >> 20);
    println!("size_kib,adler32_gibs,crc32_gibs");
    for &kib in &[64usize, 256, 1024, 4096, 16384] {
        let buf = fill(kib * 1024);
        let a = med_gibs(&buf, per_sample, samples, |d| zenflate::adler32(1, d));
        let c = med_gibs(&buf, per_sample, samples, |d| zenflate::crc32(0, d));
        println!("{kib},{a:.1},{c:.1}");
    }
}

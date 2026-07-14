//! Fresh 0.4.0 checksum-vs-C + parallel-gzip numbers for the doc refresh.
use std::hint::black_box;
use std::time::Instant;
use zenflate::{Compressor, CompressionLevel, Unstoppable};

fn med_secs(samples: usize, mut f: impl FnMut()) -> f64 {
    for _ in 0..3 { f(); }
    let mut t = Vec::with_capacity(samples);
    for _ in 0..samples { let s = Instant::now(); f(); t.push(s.elapsed().as_secs_f64()); }
    t.sort_by(f64::total_cmp);
    t[samples/2]
}
fn gibs(bytes: usize, secs: f64) -> f64 { bytes as f64 / secs / (1u64<<30) as f64 }

fn main() {
    // 1 MiB sequential (matches the CLAUDE.md / README checksum table basis)
    let n = 1<<20;
    let seq: Vec<u8> = (0..n).map(|i| (i & 0xff) as u8).collect();
    let passes = 256; // ~256 MiB per timed sample
    println!("== checksums (1 MiB sequential, {passes} passes/sample, GiB/s) ==");
    let za = med_secs(15, || { let mut a=0u32; for _ in 0..passes { a ^= zenflate::adler32(1, black_box(&seq)); } black_box(a); });
    let ca = med_secs(15, || { let mut a=0u32; for _ in 0..passes { a^=libdeflater::adler32(black_box(&seq)); } black_box(a); });
    let zc = med_secs(15, || { let mut a=0u32; for _ in 0..passes { a ^= zenflate::crc32(0, black_box(&seq)); } black_box(a); });
    let cc = med_secs(15, || { let mut a=0u32; for _ in 0..passes { a^=libdeflater::crc32(black_box(&seq)); } black_box(a); });
    let pb = passes*n;
    println!("adler32: zenflate {:.1} GiB/s | C {:.1} GiB/s | {:.2}x", gibs(pb,za), gibs(pb,ca), gibs(pb,za)/gibs(pb,ca));
    println!("crc32:   zenflate {:.1} GiB/s | C {:.1} GiB/s | {:.2}x", gibs(pb,zc), gibs(pb,cc), gibs(pb,zc)/gibs(pb,cc));

    // Parallel gzip: 4 MiB mixed data, 1 thread vs 4 threads
    let m = 4<<20;
    let mut s: u64 = 0x1234;
    let mixed: Vec<u8> = (0..m).map(|i| { s = s.wrapping_mul(6364136223846793005).wrapping_add(1); if i%32<8 {0x41} else {(s>>33) as u8} }).collect();
    println!("\n== parallel gzip (4 MiB mixed, median ms, GiB/s) ==");
    println!("effort,1T_ms,4T_ms,speedup,4T_gibs");
    for (lbl,eff) in [("e1",1u32),("e15",15),("e30",30)] {
        let t1 = med_secs(7, || {
            let mut c = Compressor::new(CompressionLevel::new(eff));
            let mut out = vec![0u8; Compressor::gzip_compress_bound(m)+64];
            black_box(c.gzip_compress(black_box(&mixed), &mut out, Unstoppable).unwrap());
        });
        let t4 = med_secs(7, || {
            let mut c = Compressor::new(CompressionLevel::new(eff));
            let mut out = vec![0u8; Compressor::gzip_compress_bound(m)+4*5+64];
            black_box(c.gzip_compress_parallel(black_box(&mixed), &mut out, 4, Unstoppable).unwrap());
        });
        println!("{lbl},{:.1},{:.1},{:.2}x,{:.3}", t1*1e3, t4*1e3, t1/t4, gibs(m,t4));
    }
}

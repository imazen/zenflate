//! zenflate vs zlib-rs: compression **ratio** and **throughput** (compress and
//! decompress) on real corpora (Canterbury, Silesia subset) plus synthetic data.
//!
//! The criterion throughput bench reports *speed* at a nominal level number; on
//! its own that is misleading (a faster compressor that produces larger output
//! is at a different point on the speed/ratio curve, not strictly "faster").
//! This harness reports ratio and speed *together*, at matched level numbers,
//! so the trade-off can be read honestly.
//!
//! zlib format throughout (`zlib_compress`/`zlib_decompress`) — that is how a
//! flate2-style backend is actually exercised. Levels map 1:1; zlib-rs clamps to
//! 9, so zenflate L10/L12 are compared against zlib-rs's max effort (9).
//!
//! Run: `cargo run --release --example zlib_rs_ratio`
//!      (add `--features unchecked` to exercise zenflate's bounds-check-free hot path)

use std::time::Instant;

// ---------------------------------------------------------------------------
// Synthetic data
// ---------------------------------------------------------------------------

fn make_mixed(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut state: u32 = 0xDEAD_BEEF;
    let mut i = 0;
    while i < size {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let byte = (state >> 16) as u8;
        if i % 256 == 0 && i + 32 <= size {
            data.extend(std::iter::repeat_n(byte, 32));
            i += 32;
        } else {
            data.push(byte);
            i += 1;
        }
    }
    data.truncate(size);
    data
}

fn make_photo_bitmap(width: usize, height: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(width * height * 3);
    let mut rng: u32 = 0x12345678;
    let mut next_rng = || -> u32 {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        rng
    };
    for y in 0..height {
        for x in 0..width {
            let fx = x as f64 / width as f64;
            let fy = y as f64 / height as f64;
            let r_base = (fx * 180.0 + fy * 60.0) as u32;
            let g_base = ((1.0 - fy) * 200.0 + fx * 40.0) as u32;
            let b_base = (fy * 220.0 + (1.0 - fx) * 30.0) as u32;
            let noise_r = (next_rng() >> 16) % 31;
            let noise_g = (next_rng() >> 16) % 31;
            let noise_b = (next_rng() >> 16) % 31;
            data.push((r_base + noise_r).min(255) as u8);
            data.push((g_base + noise_g).min(255) as u8);
            data.push((b_base + noise_b).min(255) as u8);
        }
    }
    data
}

// ---------------------------------------------------------------------------
// Real corpora (loaded from ~/.cache if present; silently skipped otherwise)
// ---------------------------------------------------------------------------

fn cache_root() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".cache/compression-corpus"))
}

/// Canterbury, concatenated into one buffer for an aggregate ratio/throughput.
fn load_canterbury_aggregate() -> Option<Vec<u8>> {
    let dir = cache_root()?.join("canterbury");
    let names = [
        "alice29.txt",
        "asyoulik.txt",
        "cp.html",
        "fields.c",
        "grammar.lsp",
        "kennedy.xls",
        "lcet10.txt",
        "plrabn12.txt",
        "ptt5",
        "sum",
        "xargs.1",
    ];
    let mut buf = Vec::new();
    for n in names {
        buf.extend(std::fs::read(dir.join(n)).ok()?);
    }
    (!buf.is_empty()).then_some(buf)
}

fn load_silesia(name: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_root()?.join("silesia").join(name)).ok()
}

// ---------------------------------------------------------------------------
// Compress / decompress one-shots (zlib format, matched)
// ---------------------------------------------------------------------------

fn zenflate_compress(c: &mut zenflate::Compressor, data: &[u8], out: &mut [u8]) -> usize {
    c.zlib_compress(data, out, zenflate::Unstoppable).unwrap()
}

fn zlib_rs_compress(data: &[u8], out: &mut [u8], level: u32) -> usize {
    let config = zlib_rs::DeflateConfig {
        level: level.min(9) as i32,
        method: zlib_rs::Method::Deflated,
        window_bits: 15, // zlib wrapper
        mem_level: 8,
        strategy: zlib_rs::Strategy::Default,
    };
    let (compressed, rc) = zlib_rs::compress_slice(out, data, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    compressed.len()
}

fn zenflate_decompress(d: &mut zenflate::Decompressor, comp: &[u8], out: &mut [u8]) -> usize {
    d.zlib_decompress(comp, out, zenflate::Unstoppable)
        .unwrap()
        .output_written
}

fn zlib_rs_decompress(comp: &[u8], out: &mut [u8]) -> usize {
    let (decompressed, rc) =
        zlib_rs::decompress_slice(out, comp, zlib_rs::InflateConfig::default());
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    decompressed.len()
}

// ---------------------------------------------------------------------------
// Timing: median throughput in MiB/s, warmup + black_box, reps scaled by size
// ---------------------------------------------------------------------------

fn mib_s(bytes: usize, mut f: impl FnMut()) -> f64 {
    // Fewer reps for big inputs to bound wall time; more for small/noisy ones.
    let reps = if bytes > 5_000_000 { 5 } else { 9 };
    for _ in 0..2 {
        std::hint::black_box(&mut f)();
    }
    let mut t: Vec<f64> = (0..reps)
        .map(|_| {
            let s = Instant::now();
            std::hint::black_box(&mut f)();
            s.elapsed().as_secs_f64()
        })
        .collect();
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = t[t.len() / 2];
    bytes as f64 / median / (1024.0 * 1024.0)
}

fn bench(name: &str, data: &[u8], levels: &[u32]) {
    println!("\n=== {name} — {} bytes ===", data.len());
    println!(
        "{:>5} | {:>9} {:>6} {:>9} {:>9} | {:>9} {:>6} {:>9} {:>9}",
        "lvl",
        "zf bytes",
        "ratio",
        "zf c MiB/s",
        "zf d MiB/s",
        "zl bytes",
        "ratio",
        "zl c MiB/s",
        "zl d MiB/s",
    );

    let zbound = zenflate::Compressor::zlib_compress_bound(data.len());
    let lbound = zlib_rs::compress_bound(data.len());
    let mut zf_out = vec![0u8; zbound];
    let mut zl_out = vec![0u8; lbound];
    let mut dec_out = vec![0u8; data.len()];

    for &level in levels {
        // --- ratio (deterministic; one compress each) ---
        let mut zc = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
        let zf_len = zenflate_compress(&mut zc, data, &mut zf_out);
        let zl_len = zlib_rs_compress(data, &mut zl_out, level);
        let zf_ratio = data.len() as f64 / zf_len as f64;
        let zl_ratio = data.len() as f64 / zl_len as f64;

        // --- compress throughput (reuse zenflate Compressor + output buffers) ---
        let zf_c = mib_s(data.len(), || {
            let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
            std::hint::black_box(zenflate_compress(&mut c, data, &mut zf_out));
        });
        let zl_c = mib_s(data.len(), || {
            std::hint::black_box(zlib_rs_compress(data, &mut zl_out, level));
        });

        // --- decompress throughput (decompress the level-`level` stream) ---
        let zf_comp = zf_out[..zf_len].to_vec();
        let zl_comp = zl_out[..zl_len].to_vec();
        let mut zd = zenflate::Decompressor::new();
        let zf_d = mib_s(data.len(), || {
            std::hint::black_box(zenflate_decompress(&mut zd, &zf_comp, &mut dec_out));
        });
        let zl_d = mib_s(data.len(), || {
            std::hint::black_box(zlib_rs_decompress(&zl_comp, &mut dec_out));
        });

        println!(
            "{level:>5} | {zf_len:>9} {zf_ratio:>5.2}x {zf_c:>9.0} {zf_d:>9.0} | \
             {zl_len:>9} {zl_ratio:>5.2}x {zl_c:>9.0} {zl_d:>9.0}",
        );
    }
}

fn main() {
    let unchecked = cfg!(feature = "unchecked");
    println!(
        "zenflate {} vs zlib-rs 0.6  |  zenflate unchecked feature: {}",
        env!("CARGO_PKG_VERSION"),
        if unchecked { "ON" } else { "off" },
    );
    println!(
        "zf = zenflate, zl = zlib-rs. c = compress, d = decompress. zlib format, levels 1:1 (zlib-rs clamps to 9)."
    );

    let levels = [1u32, 6, 9, 12];

    // Real corpora first (the honest signal); synthetic last.
    if let Some(data) = load_canterbury_aggregate() {
        bench("canterbury (11 files, aggregate)", &data, &levels);
    } else {
        println!("\n(canterbury corpus not cached — run `just corpus-download`)");
    }
    for name in ["dickens", "samba", "xml", "sao", "x-ray"] {
        if let Some(data) = load_silesia(name) {
            bench(&format!("silesia/{name}"), &data, &levels);
        }
    }
    bench("synthetic mixed (1 MB)", &make_mixed(1_000_000), &levels);
    bench(
        "synthetic photo (~1 MB RGB)",
        &make_photo_bitmap(577, 577),
        &levels,
    );

    println!("\nRead each level as a (ratio, speed) pair: compare zenflate and zlib-rs at MATCHED");
    println!(
        "RATIO (similar output size), not matched level number. zenflate L10/L12 vs zlib-rs L9."
    );
}

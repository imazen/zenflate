//! Compression-ratio comparison: zenflate vs zlib-rs on the same data and
//! the same nominal levels used by `benches/throughput.rs`.
//!
//! The throughput bench reports *speed* at a nominal level number; this
//! reports the *ratio* each library actually achieves there, so the speed
//! deltas can be read honestly (a faster compressor that produces larger
//! output is at a different point on the speed/ratio curve, not strictly
//! "faster"). Raw DEFLATE in both, levels mapped 1:1 (zlib-rs clamps to 9).
//!
//! Run: `cargo run --release --example zlib_rs_ratio`

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

fn zenflate_size(data: &[u8], level: u32) -> usize {
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
    let mut out = vec![0u8; zenflate::Compressor::deflate_compress_bound(data.len())];
    c.deflate_compress(data, &mut out, zenflate::Unstoppable).unwrap()
}

fn zlib_rs_size(data: &[u8], level: u32) -> usize {
    let mut out = vec![0u8; zlib_rs::compress_bound(data.len())];
    let config = zlib_rs::DeflateConfig {
        level: level.min(9) as i32,
        method: zlib_rs::Method::Deflated,
        window_bits: -15, // raw deflate
        mem_level: 8,
        strategy: zlib_rs::Strategy::Default,
    };
    let (compressed, rc) = zlib_rs::compress_slice(&mut out, data, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    compressed.len()
}

fn main() {
    let mut datasets: Vec<(String, Vec<u8>)> = vec![
        ("mixed (1 MB)".into(), make_mixed(1_000_000)),
        ("photo (~1 MB RGB)".into(), make_photo_bitmap(577, 577)),
    ];
    // Real compressible text (Silesia `dickens`, ~10 MB) if cached — synthetic
    // data above is near-incompressible (~1.1x) and doesn't differentiate
    // levels; real text shows the true ratio relationship.
    if let Some(home) = std::env::var_os("HOME") {
        let dickens = std::path::Path::new(&home).join(".cache/compression-corpus/silesia/dickens");
        match std::fs::read(&dickens) {
            Ok(bytes) => datasets.push(("dickens text (real, ~10 MB)".into(), bytes)),
            Err(_) => println!("(real corpus {dickens:?} not found — synthetic only)"),
        }
    }
    let levels = [1u32, 2, 4, 6, 9, 10, 12];

    // Wall-clock median over a few runs — complements the rigorous criterion
    // throughput bench; lets us read speed *at matched ratio* on real data.
    fn time_ms(mut f: impl FnMut() -> usize) -> f64 {
        for _ in 0..2 {
            std::hint::black_box(f());
        }
        let mut t: Vec<f64> = (0..7)
            .map(|_| {
                let s = std::time::Instant::now();
                std::hint::black_box(f());
                s.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        t[t.len() / 2]
    }

    for (name, data) in &datasets {
        println!("\n=== {name} — {} bytes ===", data.len());
        println!(
            "{:>5}  {:>10} {:>6} {:>8}   {:>10} {:>6} {:>8}",
            "level", "zf bytes", "ratio", "zf ms", "zl bytes", "ratio", "zl ms"
        );
        for &level in &levels {
            let zf = zenflate_size(data, level);
            let zl = zlib_rs_size(data, level);
            let zf_ms = time_ms(|| zenflate_size(data, level));
            let zl_ms = time_ms(|| zlib_rs_size(data, level));
            println!(
                "{level:>5}  {zf:>10} {:>5.2}x {zf_ms:>7.2}   {zl:>10} {:>5.2}x {zl_ms:>7.2}",
                data.len() as f64 / zf as f64,
                data.len() as f64 / zl as f64,
            );
        }
    }
    println!(
        "\nCompare at MATCHED RATIO (same output size), not matched level number."
    );
}

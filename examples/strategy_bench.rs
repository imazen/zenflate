/// Multi-library compression benchmark across 4 data types.
///
/// Compares zenflate (effort 0-30), libdeflate C (1-12), zlib-rs (1-9),
/// miniz_oxide (1-9), and fdeflate (single level, PNG-optimized).
///
/// Usage:
///   cargo run --release --features unchecked --example strategy_bench
///   cargo run --release --example strategy_bench              # safe mode
use std::time::Instant;

// ---------------------------------------------------------------------------
// Data generators
// ---------------------------------------------------------------------------

/// 2560x1440 screenshot-like bitmap (RGBA): large flat areas, text edges, UI borders.
fn make_screenshot(width: usize, height: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(width * height * 4);
    let mut rng: u32 = 0xCAFEBABE;
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        rng >> 16
    };

    for y in 0..height {
        for x in 0..width {
            let region = (y / (height / 6), x / (width / 4));
            let (r, g, b) = match region {
                (0, _) => (245, 245, 245),
                (1, 0) => (40, 44, 52),
                (1, 1..=2) => (255, 255, 255),
                (1, _) => (248, 249, 250),
                (2, 0) => (40, 44, 52),
                (2, _) => (255, 255, 255),
                (3, _) => (255, 255, 255),
                (4, _) => (250, 250, 250),
                _ => (240, 240, 240),
            };

            let noise = if (y % 20 < 2) || (x % 150 < 2) {
                (next() % 60) as i32 - 30
            } else {
                (next() % 6) as i32 - 3
            };

            data.push((r + noise).clamp(0, 255) as u8);
            data.push((g + noise).clamp(0, 255) as u8);
            data.push((b + noise).clamp(0, 255) as u8);
            data.push(255);
        }
    }
    data
}

/// 2000x2000 photo bitmap (RGB): gradients + noise simulating camera sensor.
fn make_photo(width: usize, height: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(width * height * 3);
    let mut rng: u32 = 0x12345678;
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        rng >> 16
    };
    for y in 0..height {
        for x in 0..width {
            let fx = x as f64 / width as f64;
            let fy = y as f64 / height as f64;
            let r_base = (fx * 180.0 + fy * 60.0) as u32;
            let g_base = ((1.0 - fy) * 200.0 + fx * 40.0) as u32;
            let b_base = (fy * 220.0 + (1.0 - fx) * 30.0) as u32;
            let noise_r = next() % 31;
            let noise_g = next() % 31;
            let noise_b = next() % 31;
            data.push((r_base + noise_r).min(255) as u8);
            data.push((g_base + noise_g).min(255) as u8);
            data.push((b_base + noise_b).min(255) as u8);
        }
    }
    data
}

/// Random noise: incompressible.
fn make_noise(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    let mut rng: u64 = 0xDEADBEEF_CAFEBABE;
    for chunk in data.chunks_mut(8) {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bytes = rng.to_le_bytes();
        for (dst, &src) in chunk.iter_mut().zip(bytes.iter()) {
            *dst = src;
        }
    }
    data
}

// ---------------------------------------------------------------------------
// Bench helpers
// ---------------------------------------------------------------------------

const WARMUP: usize = 1;
const ITERS: usize = 5;

struct BenchResult {
    library: &'static str,
    level: String,
    strategy: &'static str,
    size: usize,
    secs: f64,
}

fn bench_zenflate(data: &[u8], effort: u32) -> BenchResult {
    let strategy = match effort {
        0 => "Store",
        1..=4 => "Turbo",
        5..=7 => "FastHt",
        8..=10 => "Greedy",
        11..=17 => "Lazy",
        18..=22 => "Lazy2",
        _ => "NearOptimal",
    };
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(effort));
    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    for _ in 0..WARMUP {
        let _ = c
            .deflate_compress(data, &mut out, zenflate::Unstoppable)
            .unwrap();
    }
    let mut best = f64::MAX;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _ = c
            .deflate_compress(data, &mut out, zenflate::Unstoppable)
            .unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    let size = c
        .deflate_compress(data, &mut out, zenflate::Unstoppable)
        .unwrap();
    BenchResult {
        library: "zenflate",
        level: format!("e{effort}"),
        strategy,
        size,
        secs: best,
    }
}

fn bench_libdeflate(data: &[u8], level: i32) -> BenchResult {
    let strategy = match level {
        0 => "Store",
        1 => "HtGreedy",
        2..=4 => "Greedy",
        5..=7 => "Lazy",
        8..=9 => "Lazy2",
        _ => "NearOptimal",
    };
    let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
    let bound = c.deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    for _ in 0..WARMUP {
        let _ = c.deflate_compress(data, &mut out).unwrap();
    }
    let mut best = f64::MAX;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _ = c.deflate_compress(data, &mut out).unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    let size = c.deflate_compress(data, &mut out).unwrap();
    BenchResult {
        library: "libdeflate-C",
        level: format!("L{level}"),
        strategy,
        size,
        secs: best,
    }
}

fn bench_zlib_rs(data: &[u8], level: u32) -> BenchResult {
    let fl = flate2::Compression::new(level);
    let mut comp = flate2::Compress::new(fl, false);
    let mut out = vec![0u8; data.len() * 2 + 512];
    for _ in 0..WARMUP {
        comp.reset();
        comp.compress(data, &mut out, flate2::FlushCompress::Finish)
            .unwrap();
    }
    let mut best = f64::MAX;
    for _ in 0..ITERS {
        comp.reset();
        let start = Instant::now();
        comp.compress(data, &mut out, flate2::FlushCompress::Finish)
            .unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    comp.reset();
    comp.compress(data, &mut out, flate2::FlushCompress::Finish)
        .unwrap();
    let size = comp.total_out() as usize;
    BenchResult {
        library: "zlib-rs",
        level: format!("L{level}"),
        strategy: "",
        size,
        secs: best,
    }
}

fn bench_miniz_oxide(data: &[u8], level: u8) -> BenchResult {
    for _ in 0..WARMUP {
        let _ = miniz_oxide::deflate::compress_to_vec(data, level);
    }
    let mut best = f64::MAX;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _ = miniz_oxide::deflate::compress_to_vec(data, level);
        best = best.min(start.elapsed().as_secs_f64());
    }
    let out = miniz_oxide::deflate::compress_to_vec(data, level);
    BenchResult {
        library: "miniz_oxide",
        level: format!("L{level}"),
        strategy: "",
        size: out.len(),
        secs: best,
    }
}

fn bench_fdeflate(data: &[u8]) -> BenchResult {
    // fdeflate outputs zlib-wrapped data; subtract 2-byte header + 4-byte checksum
    // for raw DEFLATE size comparison.
    for _ in 0..WARMUP {
        let _ = fdeflate::compress_to_vec(data);
    }
    let mut best = f64::MAX;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _ = fdeflate::compress_to_vec(data);
        best = best.min(start.elapsed().as_secs_f64());
    }
    let out = fdeflate::compress_to_vec(data);
    BenchResult {
        library: "fdeflate",
        level: String::new(),
        strategy: "PNG-static",
        size: out.len().saturating_sub(6),
        secs: best,
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn format_speed(bytes: usize, secs: f64) -> String {
    let mib = bytes as f64 / 1_048_576.0;
    let mibs = mib / secs;
    if mibs >= 1024.0 {
        format!("{:.1} GiB/s", mibs / 1024.0)
    } else {
        format!("{:.0} MiB/s", mibs)
    }
}

fn print_table(name: &str, data: &[u8], results: &[BenchResult]) {
    let size_mib = data.len() as f64 / 1_048_576.0;
    println!("\n=== {name} ({size_mib:.2} MiB) ===\n");
    println!(
        "{:<14} {:>7}  {:<14}  {:>10}  {:>8}  {:>12}",
        "Library", "Level", "Strategy", "Size", "Ratio", "Speed"
    );
    println!("{}", "-".repeat(72));

    let mut prev_lib = "";
    for r in results {
        if !prev_lib.is_empty() && r.library != prev_lib {
            println!("{}", "-".repeat(72));
        }
        prev_lib = r.library;
        let ratio = r.size as f64 / data.len() as f64 * 100.0;
        println!(
            "{:<14} {:>7}  {:<14}  {:>10}  {:>7.2}%  {:>12}",
            r.library,
            r.level,
            r.strategy,
            r.size,
            ratio,
            format_speed(data.len(), r.secs)
        );
    }
}

fn run_suite(name: &str, data: &[u8]) {
    let mut results = Vec::new();

    // zenflate: key effort levels
    let efforts = [1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 15, 18, 22, 25, 28, 30];
    for &e in &efforts {
        results.push(bench_zenflate(data, e));
    }

    // libdeflate C: levels 1-12
    for level in 1..=12i32 {
        results.push(bench_libdeflate(data, level));
    }

    // zlib-rs (via flate2): levels 1-9
    for level in 1..=9u32 {
        results.push(bench_zlib_rs(data, level));
    }

    // miniz_oxide: levels 1-9
    for level in 1..=9u8 {
        results.push(bench_miniz_oxide(data, level));
    }

    // fdeflate: single level, PNG-optimized static Huffman
    results.push(bench_fdeflate(data));

    print_table(name, data, &results);
}

fn main() {
    let unchecked = cfg!(feature = "unchecked");
    let mode = if unchecked { "unchecked" } else { "safe" };
    println!("zenflate strategy benchmark");
    println!("Mode: {mode}\n");

    let screenshot = make_screenshot(2560, 1440);
    run_suite("Screenshot 2560x1440 RGBA", &screenshot);

    let photo = make_photo(2000, 2000);
    run_suite("Photo 2000x2000 RGB (4MP)", &photo);

    let noise = make_noise(4 * 1024 * 1024);
    run_suite("Random noise (4 MiB)", &noise);

    let zeros = vec![0u8; 4 * 1024 * 1024];
    run_suite("All zeros (4 MiB)", &zeros);
}

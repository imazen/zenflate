/// Compression ratio + speed benchmark across all effort levels 0-30.
///
/// Usage:
///   cargo run --release --example ratio_bench              # safe mode
///   cargo run --release --features unchecked --example ratio_bench  # unchecked mode
///   cargo run --release --example ratio_bench -- /path/to/file      # custom input
use std::time::Instant;

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

fn bench_zenflate(data: &[u8], level: u32) -> (usize, f64) {
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    let _ = c
        .deflate_compress(data, &mut out, zenflate::Unstoppable)
        .unwrap();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let _ = c
            .deflate_compress(data, &mut out, zenflate::Unstoppable)
            .unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    let len = c
        .deflate_compress(data, &mut out, zenflate::Unstoppable)
        .unwrap();
    (len, best)
}

fn bench_libdeflate(data: &[u8], level: i32) -> (usize, f64) {
    let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
    let bound = c.deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    let _ = c.deflate_compress(data, &mut out).unwrap();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let _ = c.deflate_compress(data, &mut out).unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    let len = c.deflate_compress(data, &mut out).unwrap();
    (len, best)
}

fn bench_flate2(data: &[u8], level: u32) -> (usize, f64) {
    let fl = flate2::Compression::new(level);
    let mut comp = flate2::Compress::new(fl, false);
    let mut out = vec![0u8; data.len() * 2];
    comp.compress(data, &mut out, flate2::FlushCompress::Finish)
        .unwrap();
    comp.reset();
    let mut best = f64::MAX;
    for _ in 0..5 {
        comp.reset();
        let start = Instant::now();
        comp.compress(data, &mut out, flate2::FlushCompress::Finish)
            .unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    comp.reset();
    comp.compress(data, &mut out, flate2::FlushCompress::Finish)
        .unwrap();
    (comp.total_out() as usize, best)
}

fn bench_miniz_oxide(data: &[u8], level: u8) -> (usize, f64) {
    let _ = miniz_oxide::deflate::compress_to_vec(data, level);
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        let _ = miniz_oxide::deflate::compress_to_vec(data, level);
        best = best.min(start.elapsed().as_secs_f64());
    }
    let out = miniz_oxide::deflate::compress_to_vec(data, level);
    (out.len(), best)
}

fn format_speed(bytes: usize, secs: f64) -> String {
    let mib = bytes as f64 / 1_048_576.0;
    let mibs = mib / secs;
    if mibs >= 1024.0 {
        format!("{:.1} GiB/s", mibs / 1024.0)
    } else {
        format!("{:.0} MiB/s", mibs)
    }
}

fn main() {
    let data = if let Some(path) = std::env::args().nth(1) {
        eprintln!("Reading: {path}");
        std::fs::read(&path).expect("failed to read input file")
    } else {
        eprintln!("Using built-in 1024x1024 photo bitmap (3 MiB RGB)");
        make_photo_bitmap(1024, 1024)
    };

    let unchecked = cfg!(feature = "unchecked");
    let mode = if unchecked { "unchecked" } else { "safe" };
    let size_mib = data.len() as f64 / 1_048_576.0;
    println!("\nMode: {mode} | Input: {size_mib:.2} MiB\n");

    println!(
        "{:<14} {:>7}  {:>14}  {:>10}  {:>8}  {:>10}",
        "Library", "Effort", "Strategy", "Size", "Ratio", "Speed"
    );
    println!("{}", "-".repeat(72));

    // zenflate: effort 1-30 (key points on the Pareto frontier)
    let effort_levels = [1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 15, 18, 22, 25, 28, 30];
    for &effort in &effort_levels {
        let level = zenflate::CompressionLevel::new(effort);
        let strategy = match effort {
            0 => "Store",
            1..=2 => "StaticTurbo",
            3..=4 => "Turbo",
            5..=7 => "FastHt",
            8..=10 => "Greedy",
            11..=17 => "Lazy",
            18..=22 => "Lazy2",
            _ => "NearOptimal",
        };
        let (len, secs) = bench_zenflate(&data, effort);
        let ratio = len as f64 / data.len() as f64 * 100.0;
        println!(
            "{:<14} {:>7}  {:>14}  {:>10}  {:>7.2}%  {:>10}",
            "zenflate", effort, strategy, len, ratio,
            format_speed(data.len(), secs)
        );
    }
    println!("{}", "-".repeat(72));

    // libdeflate C: levels 1-12
    for level in 1..=12i32 {
        let (len, secs) = bench_libdeflate(&data, level);
        let ratio = len as f64 / data.len() as f64 * 100.0;
        println!(
            "{:<14} {:>7}  {:>14}  {:>10}  {:>7.2}%  {:>10}",
            "libdeflate-C", level, "", len, ratio,
            format_speed(data.len(), secs)
        );
    }
    println!("{}", "-".repeat(72));

    // flate2: levels 1-9
    for level in 1..=9u32 {
        let (len, secs) = bench_flate2(&data, level);
        let ratio = len as f64 / data.len() as f64 * 100.0;
        println!(
            "{:<14} {:>7}  {:>14}  {:>10}  {:>7.2}%  {:>10}",
            "flate2", level, "", len, ratio,
            format_speed(data.len(), secs)
        );
    }
    println!("{}", "-".repeat(72));

    // miniz_oxide: levels 1-9
    for level in 1..=9u8 {
        let (len, secs) = bench_miniz_oxide(&data, level);
        let ratio = len as f64 / data.len() as f64 * 100.0;
        println!(
            "{:<14} {:>7}  {:>14}  {:>10}  {:>7.2}%  {:>10}",
            "miniz_oxide", level, "", len, ratio,
            format_speed(data.len(), secs)
        );
    }
}

/// Strategy benchmark: compare all effort levels across 4 data types.
///
/// Usage:
///   cargo run --release --features unchecked --example strategy_bench
use std::time::Instant;

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
            // Background: mostly flat white/light gray with some variation
            let region = (y / (height / 6), x / (width / 4));
            let (r, g, b) = match region {
                (0, _) => (245, 245, 245),     // top toolbar - light gray
                (1, 0) => (40, 44, 52),        // sidebar - dark
                (1, 1..=2) => (255, 255, 255), // main content - white
                (1, _) => (248, 249, 250),     // right panel - very light
                (2, 0) => (40, 44, 52),        // sidebar continues
                (2, _) => (255, 255, 255),     // content
                (3, _) => (255, 255, 255),     // content
                (4, _) => (250, 250, 250),     // near bottom
                _ => (240, 240, 240),          // status bar
            };

            // Add slight noise for anti-aliasing / subpixel rendering
            let noise = if (y % 20 < 2) || (x % 150 < 2) {
                // "Text" and "border" pixels: more variation
                (next() % 60) as i32 - 30
            } else {
                (next() % 6) as i32 - 3
            };

            data.push((r as i32 + noise).clamp(0, 255) as u8);
            data.push((g as i32 + noise).clamp(0, 255) as u8);
            data.push((b as i32 + noise).clamp(0, 255) as u8);
            data.push(255); // alpha
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
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = rng.to_le_bytes();
        for (dst, &src) in chunk.iter_mut().zip(bytes.iter()) {
            *dst = src;
        }
    }
    data
}

fn bench(data: &[u8], effort: u32) -> (usize, f64) {
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(effort));
    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];
    // Warmup
    let _ = c.deflate_compress(data, &mut out, zenflate::Unstoppable).unwrap();
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        let _ = c.deflate_compress(data, &mut out, zenflate::Unstoppable).unwrap();
        best = best.min(start.elapsed().as_secs_f64());
    }
    let len = c.deflate_compress(data, &mut out, zenflate::Unstoppable).unwrap();
    (len, best)
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

fn strategy_name(effort: u32) -> &'static str {
    match effort {
        0 => "Store",
        1..=2 => "StaticTurbo",
        3..=4 => "Turbo",
        5..=7 => "FastHt",
        8..=10 => "Greedy",
        11..=17 => "Lazy",
        18..=22 => "Lazy2",
        _ => "NearOptimal",
    }
}

fn run_suite(name: &str, data: &[u8]) {
    let size_mib = data.len() as f64 / 1_048_576.0;
    println!("\n=== {name} ({size_mib:.2} MiB) ===\n");
    println!(
        "{:>7}  {:<14}  {:>10}  {:>8}  {:>12}",
        "Effort", "Strategy", "Size", "Ratio", "Speed"
    );
    println!("{}", "-".repeat(58));

    let efforts = [1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 15, 18, 22, 25, 28, 30];
    for &e in &efforts {
        let (len, secs) = bench(data, e);
        let ratio = len as f64 / data.len() as f64 * 100.0;
        println!(
            "{:>7}  {:<14}  {:>10}  {:>7.2}%  {:>12}",
            e, strategy_name(e), len, ratio,
            format_speed(data.len(), secs)
        );
    }
}

fn main() {
    let unchecked = cfg!(feature = "unchecked");
    let mode = if unchecked { "unchecked" } else { "safe" };
    println!("Mode: {mode}\n");

    // 1. Screenshot >2K: 2560x1440 RGBA
    let screenshot = make_screenshot(2560, 1440);
    run_suite("Screenshot 2560x1440 RGBA", &screenshot);

    // 2. 4MP photo: 2000x2000 RGB = 12M
    let photo = make_photo(2000, 2000);
    run_suite("Photo 2000x2000 RGB (4MP)", &photo);

    // 3. Random noise: 4 MiB
    let noise = make_noise(4 * 1024 * 1024);
    run_suite("Random noise (4 MiB)", &noise);

    // 4. Zeros: 4 MiB
    let zeros = vec![0u8; 4 * 1024 * 1024];
    run_suite("All zeros (4 MiB)", &zeros);
}

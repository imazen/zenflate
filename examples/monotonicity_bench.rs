// Monotonicity benchmark: verify that higher effort never produces larger output.
//
// Tests all 31 effort levels (0-30) on real corpus images + synthetic data.
// Reports per-data tables with size, ratio, delta from previous effort, and
// flags any monotonicity violations.
//
// Usage:
//   cargo run --release --features unchecked --example monotonicity_bench
//   cargo run --release --features unchecked --example monotonicity_bench -- --quick
//   cargo run --release --features unchecked --example monotonicity_bench -- --tsv > data.tsv
//
// The --quick flag tests only efforts 0,1,3,5,7,8,9,10,12,15,20,24,30 (preset boundaries).
// The --tsv flag outputs machine-readable TSV with timing data for charting.

use std::time::Instant;

fn main() {
    // NearOptimal strategy uses deep recursion; spawn worker with large stack.
    let builder = std::thread::Builder::new()
        .name("bench".into())
        .stack_size(64 * 1024 * 1024); // 64 MiB
    let handle = builder.spawn(run).expect("failed to spawn thread");
    let code = handle.join().expect("thread panicked");
    std::process::exit(code);
}

fn run() -> i32 {
    let quick = std::env::args().any(|a| a == "--quick");
    let tsv = std::env::args().any(|a| a == "--tsv");

    // TSV mode always tests all 31 efforts (skip e0 Store since it's not compression)
    let efforts: Vec<u32> = if tsv {
        (1..=30).collect()
    } else if quick {
        vec![0, 1, 3, 5, 7, 8, 9, 10, 12, 15, 20, 24, 30]
    } else {
        (0..=30).collect()
    };

    // Collect test data: corpus images + synthetic
    let mut datasets: Vec<(String, Vec<u8>)> = Vec::new();

    // --- Corpus images ---
    eprintln!("Downloading corpus images (cached after first run)...");
    let corpus = codec_corpus::Corpus::new().expect("can't initialize codec-corpus cache");

    // gb82-sc: 10 screenshot images
    let sc_path = corpus
        .get("gb82-sc")
        .expect("can't download gb82-sc corpus");
    let mut sc_pngs = collect_pngs(&sc_path);
    sc_pngs.sort();
    for path in &sc_pngs {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let pixels = decode_png_to_raw(path);
        datasets.push((format!("sc/{name}"), pixels));
    }

    // CID22-512/validation: first 10 of ~41 diverse photos
    let cid_path = corpus
        .get("CID22/CID22-512/validation")
        .expect("can't download CID22-512/validation corpus");
    let mut cid_pngs = collect_pngs(&cid_path);
    cid_pngs.sort();
    for path in cid_pngs.iter().take(10) {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let pixels = decode_png_to_raw(path);
        datasets.push((format!("cid/{name}"), pixels));
    }

    // --- Synthetic data ---
    datasets.push((
        "synth/screenshot-2560x1440".to_string(),
        make_screenshot(2560, 1440),
    ));
    datasets.push(("synth/photo-2000x2000".to_string(), make_photo(2000, 2000)));
    datasets.push(("synth/noise-1M".to_string(), make_noise(1024 * 1024)));
    datasets.push(("synth/zeros-1M".to_string(), vec![0u8; 1024 * 1024]));

    if tsv {
        return run_tsv(&datasets, &efforts);
    }

    println!(
        "Monotonicity benchmark: {} datasets, {} effort levels\n",
        datasets.len(),
        efforts.len()
    );

    let use_fallback = std::env::args().any(|a| a == "--fallback");

    let mut total_raw_violations = 0usize;
    let mut total_fb_violations = 0usize;

    for (name, data) in &datasets {
        println!("=== {name} ({:.1} KiB raw) ===", data.len() as f64 / 1024.0,);
        if use_fallback {
            println!(
                "{:>6}  {:>12}  {:>10}  {:>10}  {:>7}  {:>10}  Status",
                "Effort", "Strategy", "RawSize", "FbSize", "Ratio", "Delta"
            );
            println!("{}", "-".repeat(78));
        } else {
            println!(
                "{:>6}  {:>12}  {:>10}  {:>7}  {:>10}  Status",
                "Effort", "Strategy", "Size", "Ratio", "Delta"
            );
            println!("{}", "-".repeat(62));
        }

        let mut prev_size: Option<usize> = None;
        let mut raw_violations = 0usize;
        let mut fb_violations = 0usize;

        for &effort in &efforts {
            let level = zenflate::CompressionLevel::new(effort);
            let bound = zenflate::Compressor::deflate_compress_bound(data.len());

            // Compress at the requested level
            let mut c = zenflate::Compressor::new(level);
            let mut out = vec![0u8; bound];
            let raw_size = c
                .deflate_compress(data, &mut out, zenflate::Unstoppable)
                .unwrap();

            // If --fallback, also compress with the monotonicity fallback chain
            let fb_size = if use_fallback {
                let mut best = raw_size;
                let mut cur = level;
                while let Some(fb_level) = cur.monotonicity_fallback() {
                    let mut fb_c = zenflate::Compressor::new(fb_level);
                    let mut fb_out = vec![0u8; bound];
                    if let Ok(s) = fb_c.deflate_compress(data, &mut fb_out, zenflate::Unstoppable) {
                        best = best.min(s);
                    }
                    cur = fb_level;
                }
                best
            } else {
                raw_size
            };

            let effective_size = fb_size;
            let ratio = effective_size as f64 / data.len() as f64 * 100.0;
            let strategy = effort_to_strategy_name(effort);

            let (delta_str, raw_viol, fb_viol) = if let Some(prev) = prev_size {
                let delta = effective_size as i64 - prev as i64;
                let raw_delta = raw_size as i64 - prev as i64;
                if delta > 0 {
                    (format!("+{delta}"), raw_delta > 0, true)
                } else if raw_size as i64 - prev as i64 > 0 {
                    // Fallback fixed it
                    (format!("{delta}"), true, false)
                } else {
                    (format!("{delta}"), false, false)
                }
            } else {
                ("--".to_string(), false, false)
            };

            if raw_viol {
                raw_violations += 1;
            }
            if fb_viol {
                fb_violations += 1;
            }

            let status = if fb_viol {
                "  <<<" // Violation even with fallback
            } else if raw_viol {
                "  (fb)" // Fixed by fallback
            } else {
                ""
            };

            if use_fallback {
                let fb_str = if fb_size < raw_size {
                    format!("{fb_size}")
                } else {
                    "-".to_string()
                };
                println!(
                    "  e{effort:<3}  {strategy:<12}  {raw_size:>10}  {fb_str:>10}  {ratio:>6.2}%  {delta_str:>10}  {status}",
                );
            } else {
                println!(
                    "  e{effort:<3}  {strategy:<12}  {effective_size:>10}  {ratio:>6.2}%  {delta_str:>10}  {status}",
                );
            }

            prev_size = Some(effective_size);
        }

        if raw_violations > 0 || fb_violations > 0 {
            if use_fallback {
                println!("  *** {raw_violations} raw, {fb_violations} after fallback ***");
            } else {
                println!("  *** {raw_violations} violation(s) ***");
            }
        }
        println!();
        total_raw_violations += raw_violations;
        total_fb_violations += fb_violations;
    }

    // Summary
    println!("========================================");
    if use_fallback {
        println!(
            "Raw violations: {total_raw_violations}, After fallback: {total_fb_violations} (across {} datasets)",
            datasets.len()
        );
        if total_fb_violations == 0 {
            println!("PASS: Fallback chain eliminates all violations");
        } else {
            println!("FAIL: {total_fb_violations} violation(s) remain after fallback");
        }
    } else if total_raw_violations == 0 {
        println!(
            "PASS: No monotonicity violations across {} datasets",
            datasets.len()
        );
    } else {
        println!(
            "FAIL: {total_raw_violations} total violation(s) across {} datasets",
            datasets.len()
        );
        println!("Hint: re-run with --fallback to see effect of monotonicity_fallback()");
    }

    if use_fallback {
        if total_fb_violations > 0 { 1 } else { 0 }
    } else if total_raw_violations > 0 {
        1
    } else {
        0
    }
}

/// TSV mode: output machine-readable data with timing for charting.
/// Columns: dataset, category, effort, strategy, raw_bytes, compressed_bytes, ratio, speed_mibps
fn run_tsv(datasets: &[(String, Vec<u8>)], efforts: &[u32]) -> i32 {
    const WARMUP: usize = 1;
    const ITERS: usize = 5;

    eprintln!(
        "TSV benchmark: {} datasets, {} effort levels, {ITERS} iterations each",
        datasets.len(),
        efforts.len()
    );

    println!(
        "dataset\tcategory\teffort\tstrategy\traw_bytes\tcompressed_bytes\tratio\tspeed_mibps"
    );

    for (name, data) in datasets {
        let category = if name.starts_with("sc/") {
            "screenshot"
        } else if name.starts_with("cid/") {
            "photo"
        } else if name.contains("noise") {
            "noise"
        } else if name.contains("zeros") {
            "zeros"
        } else if name.contains("screenshot") {
            "screenshot"
        } else {
            "photo"
        };

        eprintln!("  {name} ({:.1} KiB)...", data.len() as f64 / 1024.0);

        for &effort in efforts {
            let level = zenflate::CompressionLevel::new(effort);
            let mut c = zenflate::Compressor::new(level);
            let bound = zenflate::Compressor::deflate_compress_bound(data.len());
            let mut out = vec![0u8; bound];

            // Warmup
            for _ in 0..WARMUP {
                let _ = c.deflate_compress(data, &mut out, zenflate::Unstoppable);
            }

            // Timed runs — take best
            let mut best_secs = f64::MAX;
            let mut size = 0;
            for _ in 0..ITERS {
                let start = Instant::now();
                size = c
                    .deflate_compress(data, &mut out, zenflate::Unstoppable)
                    .unwrap();
                let elapsed = start.elapsed().as_secs_f64();
                best_secs = best_secs.min(elapsed);
            }

            let ratio = size as f64 / data.len() as f64;
            let speed_mibps = (data.len() as f64 / 1_048_576.0) / best_secs;
            let strategy = effort_to_strategy_name(effort);

            println!(
                "{name}\t{category}\t{effort}\t{strategy}\t{}\t{size}\t{ratio:.6}\t{speed_mibps:.1}",
                data.len()
            );
        }
    }

    eprintln!("Done.");
    0
}

fn effort_to_strategy_name(effort: u32) -> &'static str {
    match effort {
        0 => "Store",
        1..=4 => "Turbo",
        5..=9 => "FastHt",
        10 => "Greedy",
        11..=17 => "Lazy",
        18..=22 => "Lazy2",
        _ => "NearOptimal",
    }
}

// ---------------------------------------------------------------------------
// Synthetic data generators (same as strategy_bench.rs for reproducibility)
// ---------------------------------------------------------------------------

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
// PNG decoding
// ---------------------------------------------------------------------------

fn decode_png_to_raw(path: &std::path::Path) -> Vec<u8> {
    let file =
        std::fs::File::open(path).unwrap_or_else(|e| panic!("can't open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().expect("can't read PNG info");
    let buf_size = reader.output_buffer_size().expect("output_buffer_size unknown");
    let mut buf = vec![0u8; buf_size];
    let output_info = reader.next_frame(&mut buf).expect("can't decode frame");
    buf.truncate(output_info.buffer_size());
    buf
}

fn collect_pngs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    collect_pngs_recursive(dir, &mut out);
    out
}

fn collect_pngs_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("can't read {}: {e}", dir.display()));
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_pngs_recursive(&path, out);
        } else if path.extension().is_some_and(|e| e == "png") {
            out.push(path);
        }
    }
}

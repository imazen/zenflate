//! Image-DEFLATE corpus: zenflate vs zlib-rs on the byte streams that PNG and
//! TIFF-Deflate actually hand to the compressor.
//!
//! Text corpora (Canterbury, Silesia) are the wrong yardstick for the DEFLATE that
//! flows through image codecs. Image DEFLATE almost never compresses raw pixels — it
//! compresses *decorrelated residuals*: PNG applies a per-row adaptive filter
//! (None/Sub/Up/Average/Paeth) before DEFLATE; TIFF "Deflate"/"Adobe Deflate"
//! usually applies horizontal differencing (Predictor 2) first. The filter changes
//! the byte statistics DEFLATE sees more than the choice of compressor does, so a
//! corpus that skips the transform measures the wrong thing.
//!
//! This harness takes content-diverse images (the imazen-26 PNG set: photos,
//! textures, renders, documents, scans, plots, screenshots, clipart) and runs each
//! through three transforms, then compresses with zenflate and zlib-rs:
//!
//!   raw         row-major pixels, no predictor      (TIFF Deflate, predictor 1)
//!   tiff_pred2  horizontal byte differencing        (TIFF Adobe Deflate, common)
//!   png_filter  per-row adaptive PNG filter          (what PNG encoders feed DEFLATE)
//!
//! GIF is LZW, not DEFLATE (that's the `zenlzw` crate); palette-index streams are a
//! separate data class. This harness covers the two DEFLATE image formats (PNG, TIFF).
//!
//! Corpus dir defaults to the imazen-26 PNG conversions; override with IMG_CORPUS_DIR.
//! Sample count per class via IMG_PER_CLASS (default 3). Each image is center-cropped
//! to at most ~1.5 MP to bound wall time; the cropped size is reported.
//!
//! Run: `cargo run --release --features unchecked --example image_deflate_corpus`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Content-class mapping (imazen-26 numeric-prefix taxonomy -> coarse class)
// ---------------------------------------------------------------------------

fn class_of(dir_name: &str) -> &'static str {
    let n: u32 = dir_name
        .split(['-', '_'])
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match n {
        1000..=3999 => "photo",
        5000..=5999 => "document",
        6000..=6999 => "scan",
        7000..=7999 => "plot",
        8000..=8999 => "screenshot",
        9000..=9999 => "clipart",
        _ => "other",
    }
}

// More specific overrides where the coarse range hides a distinct class.
fn refine_class(dir_name: &str) -> &'static str {
    if dir_name.starts_with("2400") {
        "texture"
    } else if dir_name.starts_with("2200") {
        "render"
    } else if dir_name.contains("icon") {
        "icon"
    } else {
        class_of(dir_name)
    }
}

// ---------------------------------------------------------------------------
// PNG decode + center crop
// ---------------------------------------------------------------------------

struct Image {
    w: usize,
    h: usize,
    ch: usize,
    px: Vec<u8>, // row-major, w*h*ch bytes, 8-bit
}

/// Recursively collect `.png` paths under `dir` (some classes nest by sub-topic).
fn collect_pngs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_pngs(&p, out);
        } else if p.extension().is_some_and(|x| x == "png") {
            out.push(p);
        }
    }
}

fn decode_png(path: &Path) -> Option<Image> {
    let file = std::fs::File::open(path).ok()?;
    let dec = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None; // v1: 8-bit only
    }
    let ch = info.color_type.samples();
    buf.truncate(info.buffer_size());
    Some(Image {
        w: info.width as usize,
        h: info.height as usize,
        ch,
        px: buf,
    })
}

/// Center-crop to at most `cap` pixels, keeping local pixel statistics.
fn maybe_crop(img: &Image, cap: usize) -> Image {
    if img.w * img.h <= cap {
        return Image {
            w: img.w,
            h: img.h,
            ch: img.ch,
            px: img.px.clone(),
        };
    }
    // Largest square (in pixels) that fits under cap, bounded by the image.
    let side = (cap as f64).sqrt() as usize;
    let cw = side.min(img.w);
    let chh = side.min(img.h);
    let x0 = (img.w - cw) / 2;
    let y0 = (img.h - chh) / 2;
    let mut px = Vec::with_capacity(cw * chh * img.ch);
    for y in y0..y0 + chh {
        let row = &img.px[(y * img.w + x0) * img.ch..(y * img.w + x0 + cw) * img.ch];
        px.extend_from_slice(row);
    }
    Image {
        w: cw,
        h: chh,
        ch: img.ch,
        px,
    }
}

// ---------------------------------------------------------------------------
// Codec transforms
// ---------------------------------------------------------------------------

fn t_raw(img: &Image) -> Vec<u8> {
    img.px.clone()
}

/// TIFF Predictor 2: horizontal byte differencing, per component.
fn t_tiff_pred2(img: &Image) -> Vec<u8> {
    let (w, ch) = (img.w, img.ch);
    let mut out = img.px.clone();
    let rowbytes = w * ch;
    for row in out.chunks_mut(rowbytes) {
        // right-to-left so each sample subtracts its (original) left neighbour
        for x in (1..w).rev() {
            for c in 0..ch {
                let cur = row[x * ch + c];
                let left = row[(x - 1) * ch + c];
                row[x * ch + c] = cur.wrapping_sub(left);
            }
        }
    }
    out
}

/// PNG adaptive filtering: per row pick the filter (None/Sub/Up/Average/Paeth)
/// with the minimum sum of absolute signed residuals (libpng's MSAD heuristic),
/// emit a filter-type byte then the filtered row.
fn t_png_filter(img: &Image) -> Vec<u8> {
    let (w, h, ch) = (img.w, img.h, img.ch);
    let rb = w * ch;
    let bpp = ch;
    let mut out = Vec::with_capacity(h * (rb + 1));
    let mut cand = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for c in &mut cand {
        c.resize(rb, 0u8);
    }
    let zero_row = vec![0u8; rb];
    for y in 0..h {
        let cur = &img.px[y * rb..y * rb + rb];
        let prev: &[u8] = if y == 0 {
            &zero_row
        } else {
            &img.px[(y - 1) * rb..(y - 1) * rb + rb]
        };
        for i in 0..rb {
            let a = if i >= bpp { cur[i - bpp] } else { 0 }; // left
            let b = prev[i]; // up
            let cc = if i >= bpp { prev[i - bpp] } else { 0 }; // up-left
            cand[0][i] = cur[i];
            cand[1][i] = cur[i].wrapping_sub(a);
            cand[2][i] = cur[i].wrapping_sub(b);
            cand[3][i] = cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8);
            cand[4][i] = cur[i].wrapping_sub(paeth(a, b, cc));
        }
        // pick min sum-of-abs(signed)
        let mut best = 0usize;
        let mut best_cost = u64::MAX;
        for (f, row) in cand.iter().enumerate() {
            let cost: u64 = row.iter().map(|&v| (v as i8).unsigned_abs() as u64).sum();
            if cost < best_cost {
                best_cost = cost;
                best = f;
            }
        }
        out.push(best as u8);
        out.extend_from_slice(&cand[best]);
    }
    out
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (a as i16, b as i16, c as i16);
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

// ---------------------------------------------------------------------------
// Compress / decompress (zlib format, matched) — same shape as zlib_rs_ratio
// ---------------------------------------------------------------------------

fn zf_compress(level: u32, data: &[u8], out: &mut [u8]) -> usize {
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
    c.zlib_compress(data, out, zenflate::Unstoppable).unwrap()
}

fn zl_compress(level: u32, data: &[u8], out: &mut [u8]) -> usize {
    let config = zlib_rs::DeflateConfig {
        level: level.min(9) as i32,
        method: zlib_rs::Method::Deflated,
        window_bits: 15,
        mem_level: 8,
        strategy: zlib_rs::Strategy::Default,
    };
    let (compressed, rc) = zlib_rs::compress_slice(out, data, config);
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    compressed.len()
}

fn time_mib_s(bytes: usize, mut f: impl FnMut()) -> f64 {
    for _ in 0..1 {
        std::hint::black_box(&mut f)();
    }
    let mut t: Vec<f64> = (0..3)
        .map(|_| {
            let s = Instant::now();
            std::hint::black_box(&mut f)();
            s.elapsed().as_secs_f64()
        })
        .collect();
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    bytes as f64 / t[1] / (1024.0 * 1024.0)
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Agg {
    in_bytes: u64,
    zf_out: u64,
    zl_out: u64,
    zf_c_mibps: Vec<f64>,
    zl_c_mibps: Vec<f64>,
    zf_d_mibps: Vec<f64>,
    zl_d_mibps: Vec<f64>,
    n: u32,
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let corpus = std::env::var("IMG_CORPUS_DIR")
        .unwrap_or_else(|_| "/mnt/v/output/imazen-26-png".to_string());
    let per_class: usize = std::env::var("IMG_PER_CLASS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let cap_px: usize = 1_500_000;
    let levels = [6u32, 12u32]; // L12 vs zlib-rs's clamped L9 = matched max effort

    println!(
        "image-DEFLATE corpus: zenflate {} (unchecked={}) vs zlib-rs 0.6",
        env!("CARGO_PKG_VERSION"),
        cfg!(feature = "unchecked")
    );
    println!(
        "corpus: {corpus}  (per-class={per_class}, center-crop ≤{} MP)",
        cap_px / 1_000_000
    );

    // Collect class dirs.
    let mut class_dirs: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&corpus) else {
        println!(
            "\nCorpus dir not found: {corpus}\nSet IMG_CORPUS_DIR to a directory of class-subfolders of PNGs."
        );
        return;
    };
    for e in entries.flatten() {
        if e.path().is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            let class = refine_class(&name);
            let mut pngs: Vec<PathBuf> = Vec::new();
            collect_pngs(&e.path(), &mut pngs);
            pngs.sort();
            class_dirs.entry(class).or_default().extend(pngs);
        }
    }
    if class_dirs.is_empty() {
        println!("\nNo class subdirs with PNGs found under {corpus}.");
        return;
    }

    type Transform = (&'static str, fn(&Image) -> Vec<u8>);
    let transforms: [Transform; 3] = [
        ("raw", t_raw),
        ("tiff_pred2", t_tiff_pred2),
        ("png_filter", t_png_filter),
    ];

    // class -> transform -> Agg
    let mut results: BTreeMap<String, BTreeMap<&str, [Agg; 2]>> = BTreeMap::new();

    for (class, mut files) in class_dirs {
        files.sort();
        // deterministic spread: take evenly-spaced samples across the class
        let picks: Vec<&PathBuf> = if files.len() <= per_class {
            files.iter().collect()
        } else {
            let step = files.len() / per_class;
            (0..per_class).map(|i| &files[i * step]).collect()
        };
        for path in picks {
            let Some(img) = decode_png(path) else {
                continue;
            };
            if img.ch == 0 || img.ch > 4 {
                continue;
            }
            let img = maybe_crop(&img, cap_px);
            eprintln!(
                "  {class:<10} {:>5}x{:<5} ch={} {}",
                img.w,
                img.h,
                img.ch,
                path.file_name().unwrap().to_string_lossy()
            );
            for (tname, tf) in &transforms {
                let stream = tf(&img);
                let mut zf_out = vec![0u8; zenflate::Compressor::zlib_compress_bound(stream.len())];
                let mut zl_out = vec![0u8; zlib_rs::compress_bound(stream.len())];
                let mut dec = vec![0u8; stream.len()];

                let agg_slot = results
                    .entry(class.to_string())
                    .or_default()
                    .entry(tname)
                    .or_insert_with(|| [Agg::default(), Agg::default()]);

                for (li, &level) in levels.iter().enumerate() {
                    let zf_len = zf_compress(level, &stream, &mut zf_out);
                    let zl_len = zl_compress(level, &stream, &mut zl_out);
                    let a = &mut agg_slot[li];
                    a.in_bytes += stream.len() as u64;
                    a.zf_out += zf_len as u64;
                    a.zl_out += zl_len as u64;
                    a.n += 1;
                    a.zf_c_mibps.push(time_mib_s(stream.len(), || {
                        std::hint::black_box(zf_compress(level, &stream, &mut zf_out));
                    }));
                    a.zl_c_mibps.push(time_mib_s(stream.len(), || {
                        std::hint::black_box(zl_compress(level, &stream, &mut zl_out));
                    }));
                    // decompress
                    let zf_comp = zf_out[..zf_len].to_vec();
                    let zl_comp = zl_out[..zl_len].to_vec();
                    let mut zd = zenflate::Decompressor::new();
                    a.zf_d_mibps.push(time_mib_s(stream.len(), || {
                        std::hint::black_box(
                            zd.zlib_decompress(&zf_comp, &mut dec, zenflate::Unstoppable)
                                .unwrap()
                                .output_written,
                        );
                    }));
                    a.zl_d_mibps.push(time_mib_s(stream.len(), || {
                        let (d, rc) = zlib_rs::decompress_slice(
                            &mut dec,
                            &zl_comp,
                            zlib_rs::InflateConfig::default(),
                        );
                        assert_eq!(rc, zlib_rs::ReturnCode::Ok);
                        std::hint::black_box(d.len());
                    }));
                }
            }
        }
    }

    // ---- report ----
    for (li, level) in levels.iter().enumerate() {
        let zl_lvl = (*level).min(9);
        println!(
            "\n================ level: zenflate L{level} vs zlib-rs L{zl_lvl} ================"
        );
        println!(
            "{:<11} {:<11} {:>4} {:>7} {:>7} {:>8} {:>8} {:>8} {:>8}",
            "class",
            "transform",
            "n",
            "zf rat",
            "zl rat",
            "zf cMiB",
            "zl cMiB",
            "zf dMiB",
            "zl dMiB"
        );
        for tname in ["raw", "tiff_pred2", "png_filter"] {
            let mut tot = Agg::default();
            for (class, per_t) in &results {
                if let Some(slots) = per_t.get(tname) {
                    let a = slots[li].clone();
                    if a.n == 0 {
                        continue;
                    }
                    let zf_r = a.in_bytes as f64 / a.zf_out as f64;
                    let zl_r = a.in_bytes as f64 / a.zl_out as f64;
                    let mut zfc = a.zf_c_mibps.clone();
                    let mut zlc = a.zl_c_mibps.clone();
                    let mut zfd = a.zf_d_mibps.clone();
                    let mut zld = a.zl_d_mibps.clone();
                    println!(
                        "{class:<11} {tname:<11} {:>4} {zf_r:>6.2}x {zl_r:>6.2}x {:>8.0} {:>8.0} {:>8.0} {:>8.0}",
                        a.n,
                        median(&mut zfc),
                        median(&mut zlc),
                        median(&mut zfd),
                        median(&mut zld),
                    );
                    tot.in_bytes += a.in_bytes;
                    tot.zf_out += a.zf_out;
                    tot.zl_out += a.zl_out;
                    tot.n += a.n;
                    tot.zf_c_mibps.extend(a.zf_c_mibps);
                    tot.zl_c_mibps.extend(a.zl_c_mibps);
                    tot.zf_d_mibps.extend(a.zf_d_mibps);
                    tot.zl_d_mibps.extend(a.zl_d_mibps);
                }
            }
            if tot.n > 0 {
                let zf_r = tot.in_bytes as f64 / tot.zf_out as f64;
                let zl_r = tot.in_bytes as f64 / tot.zl_out as f64;
                println!(
                    "{:<11} {tname:<11} {:>4} {zf_r:>6.2}x {zl_r:>6.2}x {:>8.0} {:>8.0} {:>8.0} {:>8.0}   <== ALL",
                    "*",
                    tot.n,
                    median(&mut tot.zf_c_mibps),
                    median(&mut tot.zl_c_mibps),
                    median(&mut tot.zf_d_mibps),
                    median(&mut tot.zl_d_mibps),
                );
            }
        }
    }
    println!(
        "\nratio = original/compressed (higher is better). cMiB = compress MiB/s, dMiB = decompress MiB/s."
    );
    println!(
        "Note how the transform moves the ratio far more than the compressor does: png_filter/tiff_pred2"
    );
    println!(
        "vs raw is the real image-DEFLATE workload. Photos barely move; screenshots/clipart/plots move a lot."
    );
}

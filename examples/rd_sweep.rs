//! Ratio-vs-time frontier sweep across the Rust DEFLATE ecosystem.
//!
//! For every (library, level) point this measures compressed size AND
//! compression wall time (adaptive median-of-N), verifies the stream
//! round-trips through zenflate's decoder byte-exactly, and times that
//! decode. Output is CSV on stdout; progress goes to stderr.
//!
//! Covers the max-compression tail: zenflate efforts 31+ (Zopfli-style
//! full-optimal), the `zopfli` crate, miniz_oxide level 10 ("uber"), and
//! libdeflate L12.
//!
//! Usage:
//!   cargo run --release --example rd_sweep > benchmarks/rd_sweep_$(date +%F).csv
//!   cargo run --release --example rd_sweep -- /path/to/input ...   # extra datasets
//!
//! Built-in datasets: mixed 1MB + photo 1MB (synthetic, deterministic), plus
//! silesia dickens/xml from ~/.cache/compression-corpus/silesia/ when present.

use std::io::Write as _;
use std::num::NonZeroU64;
use std::time::Instant;

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
            let nr = (next_rng() >> 16) % 31;
            let ng = (next_rng() >> 16) % 31;
            let nb = (next_rng() >> 16) % 31;
            data.push((r_base + nr).min(255) as u8);
            data.push((g_base + ng).min(255) as u8);
            data.push((b_base + nb).min(255) as u8);
        }
    }
    data
}

type CompressFn = Box<dyn FnMut(&[u8]) -> Vec<u8>>;

#[derive(Clone, Copy, PartialEq)]
enum Fmt {
    Deflate,
    Zlib,
}

/// Adaptive-rep timing: returns (median_secs, min_secs, reps).
fn time_it(mut f: impl FnMut() -> usize) -> (f64, f64, u32) {
    // Warmup + first measurement decides the rep count.
    let t0 = Instant::now();
    let _ = f();
    let first = t0.elapsed().as_secs_f64();
    let reps: u32 = if first < 0.05 {
        9
    } else if first < 0.5 {
        5
    } else if first < 5.0 {
        3
    } else {
        1
    };
    let mut times = Vec::with_capacity(reps as usize);
    for _ in 0..reps {
        let t = Instant::now();
        let _ = f();
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    (median, times[0], reps)
}

struct Point {
    lib: &'static str,
    label: String,
    fmt: Fmt,
    compress: CompressFn,
    /// Skip datasets larger than this (optimal parsers on 10 MB inputs).
    max_input: usize,
}

fn points() -> Vec<Point> {
    let mut v: Vec<Point> = Vec::new();

    // zenflate: the effort scale, including the Zopfli-style 31+ tail.
    for e in [1u32, 2, 4, 6, 9, 10, 12, 15, 18, 22, 25, 30] {
        v.push(Point {
            lib: "zenflate",
            label: format!("e{e}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(e));
                let mut out = vec![0u8; zenflate::Compressor::deflate_compress_bound(data.len())];
                let n = c
                    .deflate_compress(data, &mut out, zenflate::Unstoppable)
                    .unwrap();
                out.truncate(n);
                out
            }),
            max_input: usize::MAX,
        });
    }
    for e in [31u32, 46, 76] {
        v.push(Point {
            lib: "zenflate",
            label: format!("e{e}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::new(e));
                let mut out = vec![0u8; zenflate::Compressor::deflate_compress_bound(data.len())];
                let n = c
                    .deflate_compress(data, &mut out, zenflate::Unstoppable)
                    .unwrap();
                out.truncate(n);
                out
            }),
            // e31/e46 run everywhere; e76 (60 iterations) only on small inputs
            max_input: if e >= 76 { 2_000_000 } else { usize::MAX },
        });
    }

    // libdeflate (C reference)
    for l in 1i32..=12 {
        v.push(Point {
            lib: "libdeflate",
            label: format!("L{l}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                let mut c =
                    libdeflater::Compressor::new(libdeflater::CompressionLvl::new(l).unwrap());
                let mut out = vec![0u8; c.deflate_compress_bound(data.len())];
                let n = c.deflate_compress(data, &mut out).unwrap();
                out.truncate(n);
                out
            }),
            max_input: usize::MAX,
        });
    }

    // zlib-rs (raw deflate)
    for l in 1i32..=9 {
        v.push(Point {
            lib: "zlib-rs",
            label: format!("L{l}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                let mut out = vec![0u8; zlib_rs::compress_bound(data.len())];
                let config = zlib_rs::DeflateConfig {
                    level: l,
                    method: zlib_rs::Method::Deflated,
                    window_bits: -15,
                    mem_level: 8,
                    strategy: zlib_rs::Strategy::Default,
                };
                let (compressed, rc) = zlib_rs::compress_slice(&mut out, data, config);
                assert_eq!(rc, zlib_rs::ReturnCode::Ok);
                let n = compressed.len();
                out.truncate(n);
                out
            }),
            max_input: usize::MAX,
        });
    }

    // miniz_oxide (10 = "uber")
    for l in 1u8..=10 {
        v.push(Point {
            lib: "miniz_oxide",
            label: format!("L{l}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| miniz_oxide::deflate::compress_to_vec(data, l)),
            max_input: usize::MAX,
        });
    }

    // yazi
    for l in 1u8..=10 {
        v.push(Point {
            lib: "yazi",
            label: format!("L{l}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                yazi::compress(data, yazi::Format::Raw, yazi::CompressionLevel::Specific(l))
                    .unwrap()
            }),
            max_input: usize::MAX,
        });
    }

    // libflate (single default configuration)
    v.push(Point {
        lib: "libflate",
        label: "default".into(),
        fmt: Fmt::Deflate,
        compress: Box::new(|data| {
            let mut enc = libflate::deflate::Encoder::new(Vec::with_capacity(data.len() / 2));
            enc.write_all(data).unwrap();
            enc.finish().into_result().unwrap()
        }),
        max_input: usize::MAX,
    });

    // fdeflate (single fast configuration; zlib-wrapped output)
    v.push(Point {
        lib: "fdeflate",
        label: "default".into(),
        fmt: Fmt::Zlib,
        compress: Box::new(fdeflate::compress_to_vec),
        max_input: usize::MAX,
    });

    // zopfli (optimal parser; default 15 iterations, plus 30) — small inputs only
    for iters in [15u64, 30] {
        v.push(Point {
            lib: "zopfli",
            label: format!("i{iters}"),
            fmt: Fmt::Deflate,
            compress: Box::new(move |data| {
                let options = zopfli::Options {
                    iteration_count: NonZeroU64::new(iters).unwrap(),
                    ..Default::default()
                };
                let mut out = Vec::with_capacity(data.len() / 2);
                zopfli::compress(options, zopfli::Format::Deflate, data, &mut out).unwrap();
                out
            }),
            max_input: 2_000_000,
        });
    }

    v
}

fn main() {
    let mut datasets: Vec<(String, Vec<u8>)> = vec![
        ("mixed-1MB".into(), make_mixed(1_000_000)),
        ("photo-1MB".into(), make_photo_bitmap(577, 577)),
    ];
    if let Ok(home) = std::env::var("HOME") {
        for name in ["dickens", "xml"] {
            if let Ok(d) = std::fs::read(format!("{home}/.cache/compression-corpus/silesia/{name}"))
            {
                datasets.push((format!("silesia-{name}"), d));
            }
        }
    }
    for arg in std::env::args().skip(1) {
        if let Ok(d) = std::fs::read(&arg) {
            let name = std::path::Path::new(&arg)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            datasets.push((name, d));
        }
    }

    println!(
        "dataset,input_bytes,lib,label,format,out_bytes,ratio,compress_median_ms,compress_min_ms,compress_reps,decode_zenflate_median_ms"
    );

    let mut dec = zenflate::Decompressor::new();
    for (name, data) in &datasets {
        eprintln!("=== {name} ({} bytes) ===", data.len());
        for p in &mut points() {
            if data.len() > p.max_input {
                continue;
            }
            let stream = (p.compress)(data);

            // Verify: zenflate must decode it back byte-exactly.
            let mut back = vec![0u8; data.len()];
            let outcome = match p.fmt {
                Fmt::Deflate => dec
                    .deflate_decompress(&stream, &mut back, zenflate::Unstoppable)
                    .unwrap(),
                Fmt::Zlib => dec
                    .zlib_decompress(&stream, &mut back, zenflate::Unstoppable)
                    .unwrap(),
            };
            assert_eq!(outcome.output_written, data.len(), "{}/{}", p.lib, p.label);
            assert_eq!(&back, data, "{}/{} roundtrip mismatch", p.lib, p.label);

            let (c_med, c_min, reps) = time_it(|| (p.compress)(data).len());
            let (d_med, _, _) = time_it(|| match p.fmt {
                Fmt::Deflate => {
                    dec.deflate_decompress(&stream, &mut back, zenflate::Unstoppable)
                        .unwrap()
                        .output_written
                }
                Fmt::Zlib => {
                    dec.zlib_decompress(&stream, &mut back, zenflate::Unstoppable)
                        .unwrap()
                        .output_written
                }
            });

            let fmt = match p.fmt {
                Fmt::Deflate => "deflate",
                Fmt::Zlib => "zlib",
            };
            println!(
                "{name},{},{},{},{fmt},{},{:.4},{:.3},{:.3},{reps},{:.3}",
                data.len(),
                p.lib,
                p.label,
                stream.len(),
                data.len() as f64 / stream.len() as f64,
                c_med * 1e3,
                c_min * 1e3,
                d_med * 1e3,
            );
            eprintln!(
                "  {}/{}: {} bytes ({:.3}x), {:.2} ms",
                p.lib,
                p.label,
                stream.len(),
                data.len() as f64 / stream.len() as f64,
                c_med * 1e3
            );
        }
    }
}

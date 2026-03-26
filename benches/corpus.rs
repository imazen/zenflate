//! Corpus benchmarks: zenflate vs ecosystem on standard compression corpuses.
//!
//! Uses Canterbury corpus (small text), Silesia corpus (large mixed), and
//! real photographs (gb82 from codec-corpus, decoded to raw RGB pixels).
//!
//! Corpus files are expected at:
//!   ~/.cache/compression-corpus/canterbury/
//!   ~/.cache/compression-corpus/silesia/
//!   ~/.cache/codec-corpus/v1/gb82/
//!
//! Run with: `cargo bench --bench corpus`

use zenbench::criterion_compat::*;
use zenbench::{criterion_group, criterion_main};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Corpus loading
// ---------------------------------------------------------------------------

fn corpus_cache_dir() -> PathBuf {
    dirs_next().join("compression-corpus")
}

fn dirs_next() -> PathBuf {
    if let Ok(v) = std::env::var("COMPRESSION_CORPUS_CACHE") {
        return PathBuf::from(v);
    }
    // ~/.cache/ on Linux, ~/Library/Caches/ on macOS
    if let Some(d) = home_dir() {
        return d.join(".cache");
    }
    PathBuf::from("/tmp/.cache")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn codec_corpus_dir() -> PathBuf {
    if let Some(d) = home_dir() {
        return d.join(".cache/codec-corpus/v1");
    }
    PathBuf::from("/tmp/.cache/codec-corpus/v1")
}

/// Load a file from the Canterbury corpus. Returns None if not found.
fn load_canterbury(name: &str) -> Option<Vec<u8>> {
    let path = corpus_cache_dir().join("canterbury").join(name);
    std::fs::read(&path).ok()
}

/// Load a file from the Silesia corpus. Returns None if not found.
fn load_silesia(name: &str) -> Option<Vec<u8>> {
    let path = corpus_cache_dir().join("silesia").join(name);
    std::fs::read(&path).ok()
}

/// Load a PNG from gb82 and decode to raw RGB pixels. Returns None if not found.
fn load_gb82_rgb(name: &str) -> Option<Vec<u8>> {
    let path = codec_corpus_dir().join("gb82").join(name);
    let file = std::fs::File::open(&path).ok()?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some(buf)
}

/// Collect all available Canterbury files (sorted by name for stable ordering).
fn canterbury_files() -> Vec<(&'static str, Vec<u8>)> {
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
    let mut files = Vec::new();
    for name in &names {
        if let Some(data) = load_canterbury(name) {
            files.push((*name, data));
        }
    }
    files
}

/// Select representative Silesia files (text, binary, structured, image).
fn silesia_files() -> Vec<(&'static str, Vec<u8>)> {
    let names = [
        "dickens", // English literature (10 MB)
        "mozilla", // tar archive (51 MB) — skip in bench, too large
        "mr",      // medical image (10 MB)
        "nci",     // chemistry database (33 MB)
        "ooffice", // office binary (6 MB)
        "osdb",    // database (10 MB)
        "reymont", // Polish text (6.6 MB)
        "samba",   // C source code (21 MB)
        "sao",     // star catalog binary (7 MB)
        "webster", // English dictionary (41 MB)
        "xml",     // XML data (5.3 MB)
        "x-ray",   // medical x-ray (8.5 MB)
    ];
    let mut files = Vec::new();
    for name in &names {
        // Skip mozilla — 51MB is too large for per-level criterion benchmarks
        if *name == "mozilla" {
            continue;
        }
        if let Some(data) = load_silesia(name) {
            files.push((*name, data));
        }
    }
    files
}

/// Load gb82 photos as raw RGB pixels.
fn gb82_files() -> Vec<(&'static str, Vec<u8>)> {
    let names = [
        ("dog", "dog-lossless.png"),
        ("city", "city-lossless.png"),
        ("flowers", "flowers-lossless.png"),
        ("grass", "grass-lossless.png"),
        ("sunset", "sunset-lossless.png"),
    ];
    let mut files = Vec::new();
    for (label, filename) in &names {
        if let Some(data) = load_gb82_rgb(filename) {
            files.push((*label, data));
        }
    }
    files
}

// ---------------------------------------------------------------------------
// Level mapping helpers (same as throughput.rs)
// ---------------------------------------------------------------------------

fn flate2_level(level: u32) -> flate2::Compression {
    match level {
        0 => flate2::Compression::none(),
        1 => flate2::Compression::fast(),
        6 => flate2::Compression::default(),
        n if n >= 9 => flate2::Compression::best(),
        n => flate2::Compression::new(n),
    }
}

fn miniz_level(level: u32) -> u8 {
    level.min(9) as u8
}

// ---------------------------------------------------------------------------
// Compress a single corpus file across all libraries at given levels
// ---------------------------------------------------------------------------

fn bench_corpus_compress(
    c: &mut Criterion,
    corpus_name: &str,
    files: &[(&str, Vec<u8>)],
    levels: &[u32],
) {
    if files.is_empty() {
        eprintln!(
            "WARNING: {corpus_name} corpus not found, skipping. \
             Download to ~/.cache/compression-corpus/{corpus_name}/"
        );
        return;
    }

    for (name, data) in files {
        let group_name = format!("corpus_compress/{corpus_name}/{name}");
        let mut group = c.benchmark_group(&group_name);
        group.throughput(Throughput::Bytes(data.len() as u64));

        for &level in levels {
            // zenflate
            group.bench_with_input(
                BenchmarkId::new("zenflate", format!("L{level}")),
                &level,
                |b, &level| {
                    let mut compressor =
                        zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
                    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
                    let mut out = vec![0u8; bound];
                    b.iter(|| {
                        compressor
                            .deflate_compress(data, &mut out, zenflate::Unstoppable)
                            .unwrap()
                    });
                },
            );

            // libdeflate (C)
            group.bench_with_input(
                BenchmarkId::new("libdeflate", format!("L{level}")),
                &level,
                |b, &level| {
                    let mut compressor = libdeflater::Compressor::new(
                        libdeflater::CompressionLvl::new(level as i32).unwrap(),
                    );
                    let bound = compressor.deflate_compress_bound(data.len());
                    let mut out = vec![0u8; bound];
                    b.iter(|| compressor.deflate_compress(data, &mut out).unwrap());
                },
            );

            // flate2
            {
                let fl = flate2_level(level);
                group.bench_with_input(
                    BenchmarkId::new("flate2", format!("L{level}")),
                    &level,
                    |b, _| {
                        let mut comp = flate2::Compress::new(fl, false);
                        let mut out = vec![0u8; data.len() * 2];
                        b.iter(|| {
                            comp.reset();
                            comp.compress(data, &mut out, flate2::FlushCompress::Finish)
                                .unwrap();
                            comp.total_out() as usize
                        });
                    },
                );
            }

            // miniz_oxide
            {
                let ml = miniz_level(level);
                group.bench_with_input(
                    BenchmarkId::new("miniz_oxide", format!("L{level}")),
                    &level,
                    |b, _| {
                        b.iter(|| miniz_oxide::deflate::compress_to_vec(data, ml));
                    },
                );
            }
        }

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Decompress corpus files (compressed at L6) across all libraries
// ---------------------------------------------------------------------------

fn bench_corpus_decompress(c: &mut Criterion, corpus_name: &str, files: &[(&str, Vec<u8>)]) {
    if files.is_empty() {
        return;
    }

    let level = 6u32;

    for (name, data) in files {
        let group_name = format!("corpus_decompress/{corpus_name}/{name}");
        let mut group = c.benchmark_group(&group_name);
        group.throughput(Throughput::Bytes(data.len() as u64));

        // Pre-compress as raw deflate
        let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
        let bound = zenflate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let clen = compressor
            .deflate_compress(data, &mut compressed, zenflate::Unstoppable)
            .unwrap();
        let compressed = &compressed[..clen];

        let ratio = clen as f64 / data.len() as f64 * 100.0;
        eprintln!(
            "  {corpus_name}/{name}: {} -> {} ({ratio:.1}%)",
            data.len(),
            clen
        );

        // Pre-compress as zlib (for fdeflate/zlib-rs)
        let zlib_bound = zenflate::Compressor::zlib_compress_bound(data.len());
        let mut compressed_zlib = vec![0u8; zlib_bound];
        let zlib_len = compressor
            .zlib_compress(data, &mut compressed_zlib, zenflate::Unstoppable)
            .unwrap();
        let compressed_zlib = &compressed_zlib[..zlib_len];

        // zenflate
        group.bench_function("zenflate", |b| {
            let mut dec = zenflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                dec.deflate_decompress(compressed, &mut out, zenflate::Unstoppable)
                    .unwrap()
            });
        });

        // libdeflate (C)
        group.bench_function("libdeflate", |b| {
            let mut dec = libdeflater::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| dec.deflate_decompress(compressed, &mut out).unwrap());
        });

        // fdeflate (zlib)
        group.bench_function("fdeflate", |b| {
            let mut dec = fdeflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                dec = fdeflate::Decompressor::new();
                let (_, produced) = dec.read(compressed_zlib, &mut out, 0, true).unwrap();
                produced
            });
        });

        // zlib-rs
        group.bench_function("zlib-rs", |b| {
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                let (decompressed, rc) = zlib_rs::decompress_slice(
                    &mut out,
                    compressed_zlib,
                    zlib_rs::InflateConfig::default(),
                );
                assert_eq!(rc, zlib_rs::ReturnCode::Ok);
                decompressed.len()
            });
        });

        // flate2
        group.bench_function("flate2", |b| {
            let mut dec = flate2::Decompress::new(false);
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                dec.reset(false);
                dec.decompress(compressed, &mut out, flate2::FlushDecompress::Finish)
                    .unwrap();
                dec.total_out() as usize
            });
        });

        // miniz_oxide
        group.bench_function("miniz_oxide", |b| {
            b.iter(|| miniz_oxide::inflate::decompress_to_vec(compressed).unwrap());
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Aggregate corpus benchmarks (total throughput across all files)
// ---------------------------------------------------------------------------

fn bench_corpus_aggregate(
    c: &mut Criterion,
    corpus_name: &str,
    files: &[(&str, Vec<u8>)],
    levels: &[u32],
) {
    if files.is_empty() {
        return;
    }

    // Concatenate all files for aggregate throughput measurement
    let total: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
    let total_size = total.len();

    let group_name = format!("corpus_aggregate/{corpus_name}");
    let mut group = c.benchmark_group(&group_name);
    group.throughput(Throughput::Bytes(total_size as u64));

    for &level in levels {
        // zenflate: compress each file individually, measure total time
        group.bench_with_input(
            BenchmarkId::new("zenflate", format!("L{level}")),
            &level,
            |b, &level| {
                let mut compressor =
                    zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
                let bound = zenflate::Compressor::deflate_compress_bound(
                    files.iter().map(|(_, d)| d.len()).max().unwrap_or(0),
                );
                let mut out = vec![0u8; bound];
                b.iter(|| {
                    let mut total_out = 0usize;
                    for (_, data) in files {
                        total_out += compressor
                            .deflate_compress(data, &mut out, zenflate::Unstoppable)
                            .unwrap();
                    }
                    total_out
                });
            },
        );

        // libdeflate (C)
        group.bench_with_input(
            BenchmarkId::new("libdeflate", format!("L{level}")),
            &level,
            |b, &level| {
                let mut compressor = libdeflater::Compressor::new(
                    libdeflater::CompressionLvl::new(level as i32).unwrap(),
                );
                let bound = compressor
                    .deflate_compress_bound(files.iter().map(|(_, d)| d.len()).max().unwrap_or(0));
                let mut out = vec![0u8; bound];
                b.iter(|| {
                    let mut total_out = 0usize;
                    for (_, data) in files {
                        total_out += compressor.deflate_compress(data, &mut out).unwrap();
                    }
                    total_out
                });
            },
        );

        // flate2
        {
            let fl = flate2_level(level);
            group.bench_with_input(
                BenchmarkId::new("flate2", format!("L{level}")),
                &level,
                |b, _| {
                    let max_size = files.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
                    let mut comp = flate2::Compress::new(fl, false);
                    let mut out = vec![0u8; max_size * 2];
                    b.iter(|| {
                        let mut total_out = 0usize;
                        for (_, data) in files {
                            comp.reset();
                            comp.compress(data, &mut out, flate2::FlushCompress::Finish)
                                .unwrap();
                            total_out += comp.total_out() as usize;
                        }
                        total_out
                    });
                },
            );
        }
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Top-level benchmark functions
// ---------------------------------------------------------------------------

fn bench_canterbury(c: &mut Criterion) {
    let files = canterbury_files();
    let levels = [1u32, 6, 12];
    bench_corpus_compress(c, "canterbury", &files, &levels);
    bench_corpus_decompress(c, "canterbury", &files);
    bench_corpus_aggregate(c, "canterbury", &files, &levels);
}

fn bench_silesia(c: &mut Criterion) {
    let files = silesia_files();
    let levels = [1u32, 6, 12];
    bench_corpus_compress(c, "silesia", &files, &levels);
    bench_corpus_decompress(c, "silesia", &files);
    bench_corpus_aggregate(c, "silesia", &files, &levels);
}

fn bench_photos(c: &mut Criterion) {
    let files = gb82_files();
    let levels = [1u32, 6, 12];
    bench_corpus_compress(c, "photo_rgb", &files, &levels);
    bench_corpus_decompress(c, "photo_rgb", &files);
    bench_corpus_aggregate(c, "photo_rgb", &files, &levels);
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

criterion_group!(corpus_benches, bench_canterbury, bench_silesia, bench_photos);
criterion_main!(corpus_benches);

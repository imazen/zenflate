//! Throughput benchmarks: zenflate vs ecosystem compression libraries.
//!
//! Compares: zenflate (Rust), libdeflate (C), flate2 (miniz_oxide backend),
//! and miniz_oxide (direct).
//!
//! Run with: `cargo bench --release`

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

// ---------------------------------------------------------------------------
// Test data generators
// ---------------------------------------------------------------------------

fn make_sequential(size: usize) -> Vec<u8> {
    (0..=255u8).cycle().take(size).collect()
}

fn make_zeros(size: usize) -> Vec<u8> {
    vec![0u8; size]
}

fn make_mixed(size: usize) -> Vec<u8> {
    // LCG-based pseudo-random with runs of repeats mixed in
    let mut data = Vec::with_capacity(size);
    let mut state: u32 = 0xDEAD_BEEF;
    let mut i = 0;
    while i < size {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let byte = (state >> 16) as u8;
        // Every 256 bytes, insert a run of 32 identical bytes
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
    // Generate a deterministic photo-like RGB bitmap: smooth gradients with
    // local noise, similar to natural image pixel data. Neighboring pixels
    // are correlated (like real photos) but with enough entropy to exercise
    // the compressor properly.
    let mut data = Vec::with_capacity(width * height * 3);
    let mut rng: u32 = 0x12345678;
    let mut next_rng = || -> u32 {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        rng
    };

    for y in 0..height {
        for x in 0..width {
            // Smooth gradient base (simulates sky/ground/color regions)
            let fx = x as f64 / width as f64;
            let fy = y as f64 / height as f64;
            let r_base = (fx * 180.0 + fy * 60.0) as u32;
            let g_base = ((1.0 - fy) * 200.0 + fx * 40.0) as u32;
            let b_base = (fy * 220.0 + (1.0 - fx) * 30.0) as u32;

            // Local noise (simulates texture/detail)
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
// Level mapping helpers
// ---------------------------------------------------------------------------

/// Map zenflate levels (1/6/12) to flate2 Compression.
/// flate2 max is 9, so L12 maps to best() = 9.
fn flate2_level(level: u32) -> flate2::Compression {
    match level {
        0 => flate2::Compression::none(),
        1 => flate2::Compression::fast(),
        6 => flate2::Compression::default(),
        n if n >= 9 => flate2::Compression::best(),
        n => flate2::Compression::new(n),
    }
}

/// Map zenflate levels to miniz_oxide levels (0-10).
/// L12 maps to 9 (best standard; level 10 is "uber" and very slow).
fn miniz_level(level: u32) -> u8 {
    level.min(9) as u8
}

// ---------------------------------------------------------------------------
// Compression benchmarks
// ---------------------------------------------------------------------------

fn bench_compress(c: &mut Criterion) {
    let size = 1_000_000;
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("sequential", make_sequential(size)),
        ("zeros", make_zeros(size)),
        ("mixed", make_mixed(size)),
        ("photo", make_photo_bitmap(577, 577)), // ~1MB RGB bitmap
    ];

    // Key levels from each strategy tier
    let levels = [1u32, 2, 4, 6, 9, 10, 12];

    for (name, data) in &inputs {
        let mut group = c.benchmark_group(format!("compress/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        for &level in &levels {
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
                            .unwrap();
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
                    b.iter(|| {
                        compressor.deflate_compress(data, &mut out).unwrap();
                    });
                },
            );

            // flate2 (miniz_oxide backend, reuses compressor with reset)
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

            // miniz_oxide (direct, allocates per call)
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
// Decompression benchmarks
// ---------------------------------------------------------------------------

fn bench_decompress(c: &mut Criterion) {
    let size = 1_000_000;
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("sequential", make_sequential(size)),
        ("zeros", make_zeros(size)),
        ("mixed", make_mixed(size)),
        ("photo", make_photo_bitmap(577, 577)),
    ];

    // Compress at level 6 (default) and then benchmark decompression
    let level = 6u32;

    for (name, data) in &inputs {
        let mut group = c.benchmark_group(format!("decompress/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        // Pre-compress as raw deflate (for zenflate/libdeflate/flate2/miniz_oxide)
        let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
        let bound = zenflate::Compressor::deflate_compress_bound(data.len());
        let mut compressed_deflate = vec![0u8; bound];
        let deflate_len = compressor
            .deflate_compress(data, &mut compressed_deflate, zenflate::Unstoppable)
            .unwrap();
        let compressed_deflate = &compressed_deflate[..deflate_len];

        // Pre-compress as zlib (for fdeflate and zlib-rs)
        let zlib_bound = zenflate::Compressor::zlib_compress_bound(data.len());
        let mut compressed_zlib = vec![0u8; zlib_bound];
        let zlib_len = compressor
            .zlib_compress(data, &mut compressed_zlib, zenflate::Unstoppable)
            .unwrap();
        let compressed_zlib = &compressed_zlib[..zlib_len];

        // zenflate decompression
        group.bench_function("zenflate", |b| {
            let mut decompressor = zenflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor
                    .deflate_decompress(compressed_deflate, &mut out, zenflate::Unstoppable)
                    .unwrap();
            });
        });

        // libdeflate (C) decompression
        group.bench_function("libdeflate", |b| {
            let mut decompressor = libdeflater::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor
                    .deflate_decompress(compressed_deflate, &mut out)
                    .unwrap();
            });
        });

        // fdeflate decompression (zlib format, reuses decompressor)
        group.bench_function("fdeflate", |b| {
            let mut decompressor = fdeflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor = fdeflate::Decompressor::new();
                let (_, produced) = decompressor
                    .read(compressed_zlib, &mut out, 0, true)
                    .unwrap();
                produced
            });
        });

        // zlib-rs decompression (zlib format)
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

        // flate2 decompression (reuses decompressor with reset)
        group.bench_function("flate2", |b| {
            let mut decomp = flate2::Decompress::new(false);
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decomp.reset(false);
                decomp
                    .decompress(
                        compressed_deflate,
                        &mut out,
                        flate2::FlushDecompress::Finish,
                    )
                    .unwrap();
                decomp.total_out() as usize
            });
        });

        // miniz_oxide decompression (allocates per call)
        group.bench_function("miniz_oxide", |b| {
            b.iter(|| {
                miniz_oxide::inflate::decompress_to_vec(compressed_deflate).unwrap();
            });
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Checksum benchmarks
// ---------------------------------------------------------------------------

fn bench_checksums(c: &mut Criterion) {
    let size = 1_000_000;
    let data = make_sequential(size);

    {
        let mut group = c.benchmark_group("checksum/adler32");
        group.throughput(Throughput::Bytes(data.len() as u64));

        group.bench_function("zenflate", |b| {
            b.iter(|| zenflate::adler32(1, &data));
        });

        group.bench_function("libdeflate", |b| {
            b.iter(|| libdeflater::adler32(&data));
        });

        group.bench_function("simd-adler32", |b| {
            let mut h = simd_adler32::Adler32::new();
            b.iter(|| {
                h = simd_adler32::Adler32::new();
                h.write(&data);
                h.finish()
            });
        });

        group.finish();
    }

    {
        let mut group = c.benchmark_group("checksum/crc32");
        group.throughput(Throughput::Bytes(data.len() as u64));

        group.bench_function("zenflate", |b| {
            b.iter(|| zenflate::crc32(0, &data));
        });

        group.bench_function("libdeflate", |b| {
            b.iter(|| libdeflater::crc32(&data));
        });

        group.bench_function("crc32fast", |b| {
            b.iter(|| crc32fast::hash(&data));
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Parallel compression benchmarks
// ---------------------------------------------------------------------------

fn bench_parallel_compress(c: &mut Criterion) {
    // Use 4MB to give parallel enough work per thread.
    let size = 4_000_000;
    let data = make_mixed(size);
    let levels = [1u32, 6, 12];
    let thread_counts = [1, 2, 4];

    for &level in &levels {
        let mut group = c.benchmark_group(format!("parallel/L{level}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        for &threads in &thread_counts {
            let label = if threads == 1 {
                "1T (baseline)".to_string()
            } else {
                format!("{threads}T")
            };
            group.bench_function(&label, |b| {
                let level = zenflate::CompressionLevel::new(level);
                let bound = zenflate::Compressor::gzip_compress_bound(data.len()) + threads * 5;
                let mut out = vec![0u8; bound];
                b.iter(|| {
                    let mut compressor = zenflate::Compressor::new(level);
                    compressor
                        .gzip_compress_parallel(&data, &mut out, threads, zenflate::Unstoppable)
                        .unwrap();
                });
            });
        }

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Streaming decompression benchmarks
// ---------------------------------------------------------------------------

fn bench_stream_decompress(c: &mut Criterion) {
    let size = 1_000_000;
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("sequential", make_sequential(size)),
        ("zeros", make_zeros(size)),
        ("mixed", make_mixed(size)),
        ("photo", make_photo_bitmap(577, 577)),
    ];

    let level = 6u32;

    for (name, data) in &inputs {
        let mut group = c.benchmark_group(format!("stream_decompress/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        // Pre-compress as raw deflate
        let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
        let bound = zenflate::Compressor::deflate_compress_bound(data.len());
        let mut compressed_deflate = vec![0u8; bound];
        let deflate_len = compressor
            .deflate_compress(data, &mut compressed_deflate, zenflate::Unstoppable)
            .unwrap();
        let compressed_deflate = &compressed_deflate[..deflate_len];

        // Pre-compress as zlib (for fdeflate)
        let zlib_bound = zenflate::Compressor::zlib_compress_bound(data.len());
        let mut compressed_zlib = vec![0u8; zlib_bound];
        let zlib_len = compressor
            .zlib_compress(data, &mut compressed_zlib, zenflate::Unstoppable)
            .unwrap();
        let compressed_zlib = &compressed_zlib[..zlib_len];

        // zenflate whole-buffer (baseline)
        group.bench_function("zenflate_whole", |b| {
            let mut decompressor = zenflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor
                    .deflate_decompress(compressed_deflate, &mut out, zenflate::Unstoppable)
                    .unwrap();
            });
        });

        // zenflate streaming (large capacity — measure overhead vs whole-buffer)
        group.bench_function("zenflate_stream", |b| {
            b.iter(|| {
                let mut dec = zenflate::StreamDecompressor::deflate(compressed_deflate, 64 * 1024);
                let mut total = 0;
                while !dec.is_done() {
                    dec.fill().unwrap();
                    let n = dec.peek().len();
                    total += n;
                    dec.advance(n);
                }
                total
            });
        });

        // zenflate streaming (small capacity — exercise compaction)
        group.bench_function("zenflate_stream_4k", |b| {
            b.iter(|| {
                let mut dec = zenflate::StreamDecompressor::deflate(compressed_deflate, 4096);
                let mut total = 0;
                while !dec.is_done() {
                    dec.fill().unwrap();
                    let n = dec.peek().len();
                    total += n;
                    dec.advance(n);
                }
                total
            });
        });

        // fdeflate (streaming, zlib format)
        group.bench_function("fdeflate", |b| {
            let mut decompressor = fdeflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor = fdeflate::Decompressor::new();
                let (_, produced) = decompressor
                    .read(compressed_zlib, &mut out, 0, true)
                    .unwrap();
                produced
            });
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_compress,
    bench_decompress,
    bench_stream_decompress,
    bench_checksums,
    bench_parallel_compress
);
criterion_main!(benches);

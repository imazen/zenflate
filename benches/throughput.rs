//! Throughput benchmarks: zenflate (Rust) vs libdeflate (C).
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

// ---------------------------------------------------------------------------
// Compression benchmarks
// ---------------------------------------------------------------------------

fn bench_compress(c: &mut Criterion) {
    let size = 1_000_000;
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("sequential", make_sequential(size)),
        ("zeros", make_zeros(size)),
        ("mixed", make_mixed(size)),
    ];

    // Key levels: 1 (fastest), 6 (default), 12 (best)
    let levels = [1u32, 6, 12];

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
                        compressor.deflate_compress(data, &mut out).unwrap();
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
    ];

    // Compress at level 6 (default) and then benchmark decompression
    let level = 6u32;

    for (name, data) in &inputs {
        let mut group = c.benchmark_group(format!("decompress/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        // Pre-compress with zenflate
        let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
        let bound = zenflate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let compressed_len = compressor.deflate_compress(data, &mut compressed).unwrap();
        let compressed = &compressed[..compressed_len];

        // zenflate decompression
        group.bench_function("zenflate", |b| {
            let mut decompressor = zenflate::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor.deflate_decompress(compressed, &mut out).unwrap();
            });
        });

        // libdeflate (C) decompression
        group.bench_function("libdeflate", |b| {
            let mut decompressor = libdeflater::Decompressor::new();
            let mut out = vec![0u8; data.len()];
            b.iter(|| {
                decompressor
                    .deflate_decompress(compressed, &mut out)
                    .unwrap();
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

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

criterion_group!(benches, bench_compress, bench_decompress, bench_checksums);
criterion_main!(benches);

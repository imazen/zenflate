# zenflate

Pure Rust DEFLATE/zlib/gzip compression and decompression, ported from [libdeflate](https://github.com/ebiggers/libdeflate).

Buffer-to-buffer only (no streaming). Supports compression levels 0-12. `no_std` compatible with `alloc`.

## Usage

```toml
[dependencies]
zenflate = "0.1"
```

### Compress

```rust
use zenflate::{Compressor, CompressionLevel};

let data = b"Hello, World! Hello, World! Hello, World!";
let mut compressor = Compressor::new(CompressionLevel::DEFAULT);

let bound = Compressor::deflate_compress_bound(data.len());
let mut compressed = vec![0u8; bound];
let compressed_len = compressor
    .deflate_compress(data, &mut compressed)
    .unwrap();
let compressed = &compressed[..compressed_len];
```

### Decompress

```rust
use zenflate::Decompressor;

let mut decompressor = Decompressor::new();
let mut output = vec![0u8; original_len];
let decompressed_len = decompressor
    .deflate_decompress(compressed, &mut output)
    .unwrap();
```

### Formats

All three DEFLATE-based formats are supported:

```rust
// Raw DEFLATE
compressor.deflate_compress(data, &mut out)?;
decompressor.deflate_decompress(compressed, &mut out)?;

// zlib (2-byte header + DEFLATE + Adler-32)
compressor.zlib_compress(data, &mut out)?;
decompressor.zlib_decompress(compressed, &mut out)?;

// gzip (10-byte header + DEFLATE + CRC-32)
compressor.gzip_compress(data, &mut out)?;
decompressor.gzip_decompress(compressed, &mut out)?;
```

### Compression levels

| Level | Strategy | Speed vs ratio |
|-------|----------|---------------|
| 0 | Uncompressed | No compression, just framing |
| 1 | Fastest (hash table) | Best throughput |
| 2-3 | Greedy | |
| 4-6 | Lazy | Good balance (6 is default) |
| 7-9 | Lazy2 (double lazy eval) | Better ratio |
| 10-12 | Near-optimal parsing | Best ratio, slowest |

```rust
use zenflate::CompressionLevel;

CompressionLevel::NONE     // 0
CompressionLevel::FASTEST  // 1
CompressionLevel::DEFAULT  // 6
CompressionLevel::BEST     // 12
```

Reuse `Compressor` and `Decompressor` across calls to avoid re-initialization.

## Features

- `std` (default) — enables `std::error::Error` impls
- `alloc` (included by `std`) — enables compression (requires heap allocation for matchfinder tables)

Decompression works in `no_std` without `alloc`; all state is stack-allocated.

## Performance

Benchmarked on x86_64 with AVX-512 (Intel), `--features unchecked`.
Run `cargo bench --features unchecked` to reproduce.

**Compression** (3 MiB photo bitmap, reproducible via `examples/ratio_bench.rs`):

| Library | Level | Ratio | Safe | Unchecked | vs C |
|---------|-------|-------|------|-----------|------|
| **zenflate** | 1 (fastest) | 91.69% | 134 MiB/s | 149 MiB/s | 0.81x |
| **zenflate** | 6 (lazy) | 92.31% | 102 MiB/s | 105 MiB/s | 0.88x |
| **zenflate** | 9 (lazy2) | 92.31% | 102 MiB/s | 104 MiB/s | 0.87x |
| **zenflate** | 10 (near-opt) | 91.97% | 38 MiB/s | 47 MiB/s | 0.87x |
| **zenflate** | 12 (best) | 91.80% | 33 MiB/s | 39 MiB/s | 0.89x |
| libdeflate (C) | 1 | 91.69% | — | 185 MiB/s | — |
| libdeflate (C) | 9 | 92.31% | — | 119 MiB/s | — |
| libdeflate (C) | 12 | 91.80% | — | 44 MiB/s | — |
| flate2 | 1 | 91.70% | — | 291 MiB/s | — |
| flate2 | 9 (best) | 91.58% | — | 55 MiB/s | — |
| miniz_oxide | 9 (best) | 91.58% | — | 55 MiB/s | — |

zenflate and libdeflate produce **byte-identical output** at every level.
zenflate L6-9 runs **~2x faster** than flate2/miniz_oxide at comparable ratios.
The `unchecked` feature helps most at L10-12 (+18-24%), less at L1-9 (+2-11%).

**Decompression** (compressed at L6):

| Data type | zenflate | libdeflate (C) | flate2 | miniz_oxide |
|-----------|----------|----------------|--------|-------------|
| Sequential | 27.7 GiB/s | 31.6 GiB/s | 7.2 GiB/s | 6.6 GiB/s |
| Zeros | 34.6 GiB/s | 14.5 GiB/s | 26.6 GiB/s | 17.2 GiB/s |
| Mixed | 717 MiB/s | 795 MiB/s | 585 MiB/s | 571 MiB/s |

zenflate decompression is **4x faster** than flate2/miniz_oxide on typical data.
Zeros decompression is 2.4x faster than C (Rust's `fill()` auto-vectorizes).

**Checksums:**

| Algorithm | zenflate | libdeflate (C) | Implementation |
|-----------|----------|----------------|----------------|
| Adler-32 | 105 GiB/s | 120 GiB/s | AVX2 (x86), NEON (aarch64) |
| CRC-32 | 78 GiB/s | 77 GiB/s | PCLMULQDQ (x86), PMULL (aarch64) |

**Parallel compression** (4 MB mixed data, gzip):

| Level | 1 thread | 4 threads | Speedup |
|-------|----------|-----------|---------|
| L1 | 161 MiB/s | 534 MiB/s | 3.3x |
| L6 | 133 MiB/s | 440 MiB/s | 3.3x |
| L12 | 46 MiB/s | 135 MiB/s | 2.9x |

## How it works

This is a line-by-line port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust (`#![forbid(unsafe_code)]` by default). The algorithms are identical: same matchfinders (hash table, hash chains, binary trees), same Huffman construction, same block splitting heuristics, same near-optimal parser.

SIMD acceleration for checksums (AVX2/PCLMULQDQ on x86, NEON/PMULL on aarch64) and decompression. Runtime feature detection via [archmage](https://crates.io/crates/archmage) with zero `unsafe`.

## License

MIT

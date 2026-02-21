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
use zenflate::{Compressor, CompressionLevel, Unstoppable};

let data = b"Hello, World! Hello, World! Hello, World!";
let mut compressor = Compressor::new(CompressionLevel::balanced());

let bound = Compressor::deflate_compress_bound(data.len());
let mut compressed = vec![0u8; bound];
let compressed_len = compressor
    .deflate_compress(data, &mut compressed, Unstoppable)
    .unwrap();
let compressed = &compressed[..compressed_len];
```

### Decompress

```rust
use zenflate::{Decompressor, Unstoppable};

let mut decompressor = Decompressor::new();
let mut output = vec![0u8; original_len];
let result = decompressor
    .deflate_decompress(compressed, &mut output, Unstoppable)
    .unwrap();
// result.input_consumed — bytes of compressed data consumed
// result.output_written — bytes of decompressed data produced
```

### Formats

All three DEFLATE-based formats are supported:

```rust
// Raw DEFLATE
compressor.deflate_compress(data, &mut out, Unstoppable)?;
decompressor.deflate_decompress(compressed, &mut out, Unstoppable)?;

// zlib (2-byte header + DEFLATE + Adler-32)
compressor.zlib_compress(data, &mut out, Unstoppable)?;
decompressor.zlib_decompress(compressed, &mut out, Unstoppable)?;

// gzip (10-byte header + DEFLATE + CRC-32)
compressor.gzip_compress(data, &mut out, Unstoppable)?;
decompressor.gzip_decompress(compressed, &mut out, Unstoppable)?;
```

### Compression levels

| Level | Strategy | Speed vs ratio |
|-------|----------|---------------|
| 0 | Uncompressed | No compression, just framing |
| 1 | Fastest (hash table) | Best throughput | `fastest()` |
| 2-4 | Greedy | | `fast()` (L4) |
| 5-7 | Lazy | Good balance | `balanced()` (L6, default) |
| 8-9 | Lazy2 (double lazy eval) | Better ratio | `high()` (L9) |
| 10-12 | Near-optimal parsing | Best ratio, ~3x slower | `best()` (L12) |

```rust
use zenflate::CompressionLevel;

CompressionLevel::none()      // 0 — store
CompressionLevel::fastest()   // 1 — hash table
CompressionLevel::fast()      // 4 — greedy
CompressionLevel::balanced()  // 6 — lazy (default)
CompressionLevel::high()      // 9 — lazy2
CompressionLevel::best()      // 12 — near-optimal
CompressionLevel::new(3)      // specific level
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
| Adler-32 | 114 GiB/s | 121 GiB/s | AVX-512 VNNI (x86), dotprod (aarch64), WASM simd128 |
| CRC-32 | 78 GiB/s | 77 GiB/s | PCLMULQDQ (x86), PMULL (aarch64) |

**Parallel compression** (4 MB mixed data, gzip):

| Level | 1 thread | 4 threads | Speedup |
|-------|----------|-----------|---------|
| L1 | 161 MiB/s | 534 MiB/s | 3.3x |
| L6 | 133 MiB/s | 440 MiB/s | 3.3x |
| L12 | 46 MiB/s | 135 MiB/s | 2.9x |

## How it works

This is a line-by-line port of Eric Biggers' [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust (`#![forbid(unsafe_code)]` by default). The algorithms are identical: same matchfinders (hash table, hash chains, binary trees), same Huffman construction, same block splitting heuristics, same near-optimal parser. zenflate produces byte-identical output to libdeflate at every compression level.

The C original is faster — zenflate runs at roughly 0.8-0.9x the speed of libdeflate depending on compression level and data (see benchmarks above). The gap comes from Rust's fat pointers, bounds checking, and register pressure differences. The `unchecked` feature closes some of this gap by eliding bounds checks in hot paths.

Parallel gzip compression is a zenflate addition — libdeflate is single-threaded. zenflate uses pigz-style chunking with dictionary overlap and combined CRC-32 for near-linear scaling.

SIMD acceleration for checksums (AVX2/PCLMULQDQ on x86, NEON/PMULL on aarch64) and decompression. Runtime feature detection via [archmage](https://crates.io/crates/archmage) with zero `unsafe`.

## License

MIT

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

Benchmarked against libdeflate (C) via the `libdeflater` crate. Tested on x86_64 with AVX-512 (Intel).
Run `cargo bench` to reproduce.

**Compression throughput** (1 MB sequential data):

| Level | zenflate | libdeflate (C) | Ratio |
|-------|----------|----------------|-------|
| 1 | 1.28 GiB/s | 1.46 GiB/s | 88% |
| 6 | 745 MiB/s | 871 MiB/s | 86% |
| 12 | 44 MiB/s | 73 MiB/s | 60% |

**Decompression throughput** (1 MB, compressed at L6):

| Data type | zenflate | libdeflate (C) | Ratio |
|-----------|----------|----------------|-------|
| Sequential | 23.5 GiB/s | 32.2 GiB/s | 73% |
| Zeros (RLE) | 27.8 GiB/s | 14.5 GiB/s | 192% |
| Mixed | 539 MiB/s | 799 MiB/s | 67% |

**Checksums** (1 MB):

| Checksum | zenflate | libdeflate (C) | Notes |
|----------|----------|----------------|-------|
| Adler-32 | 75 GiB/s | 124 GiB/s | AVX2 vs AVX-512 VNNI |
| CRC-32 | 2.7 GiB/s | 78 GiB/s | Scalar vs PCLMULQDQ |

Compression throughput is 60-88% of C depending on level. The gap comes from safe Rust overhead (bounds checks, no raw pointer arithmetic) and limited SIMD coverage (AVX2 Adler-32 only; CRC-32 is scalar because PCLMULQDQ isn't exposed by the archmage token system yet).

Decompression of RLE-heavy data (zeros) is faster than C thanks to Rust's `fill()` auto-vectorization.

## How it works

This is a line-by-line port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust (`#![forbid(unsafe_code)]`). The algorithms are identical: same matchfinders (hash table, hash chains, binary trees), same Huffman construction, same block splitting heuristics, same near-optimal parser.

SIMD dispatch uses [archmage](https://crates.io/crates/archmage) for runtime feature detection with zero `unsafe`.

## License

MIT

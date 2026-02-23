# zenflate

Pure Rust DEFLATE/zlib/gzip compression and decompression, ported from [libdeflate](https://github.com/ebiggers/libdeflate).

`no_std` compatible (`alloc` required for compression and streaming decompression; decompression is fully stack-allocated).

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

### Streaming decompression

For inputs that don't fit in memory or arrive incrementally. Works with
`&[u8]` (zero overhead) or any `std::io::BufRead` via `BufReadSource`.

```rust
use zenflate::{StreamDecompressor, InputSource};

// From a slice (no_std compatible):
let mut stream = StreamDecompressor::new_deflate(compressed_data);
loop {
    let chunk = stream.fill()?;
    if chunk.is_empty() { break; }
    // process chunk...
    let n = chunk.len();
    stream.advance(n);
}

// From a BufRead (std only):
use zenflate::BufReadSource;
let file = std::io::BufReader::new(std::fs::File::open("data.gz").unwrap());
let mut stream = StreamDecompressor::new_gzip(BufReadSource::new(file));
// stream also implements Read + BufRead
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

Pick a preset or dial in a specific effort from 0 to 30:

```rust
use zenflate::CompressionLevel;

// Named presets
CompressionLevel::none()      // effort 0  — store (no compression)
CompressionLevel::fastest()   // effort 1  — turbo hash table
CompressionLevel::fast()      // effort 10 — greedy hash chains
CompressionLevel::balanced()  // effort 15 — lazy matching (default)
CompressionLevel::high()      // effort 22 — double-lazy matching
CompressionLevel::best()      // effort 30 — near-optimal parsing

// Fine-grained control (0-30, clamped)
CompressionLevel::new(12)     // lazy matching, mid-range
CompressionLevel::new(25)     // near-optimal, fast end

// Byte-identical C libdeflate compatibility (0-12)
CompressionLevel::libdeflate(6)
```

| Preset | Effort | Strategy | Description |
|--------|--------|----------|-------------|
| `none()` | 0 | Store | Framing only, no compression |
| `fastest()` | 1 | Turbo | Maximum throughput |
| `fast()` | 10 | Greedy | Hash chains — big ratio jump over turbo |
| `balanced()` | 15 | Lazy | Lazy matching — good default |
| `high()` | 22 | Lazy2 | Double-lazy — best before near-optimal |
| `best()` | 30 | Near-optimal | Best compression ratio |

Effort levels 0-30 map to six strategies:

| Effort | Strategy | Notes |
|--------|----------|-------|
| 0 | Store | No compression |
| 1-4 | Turbo | Single-entry hash table, fastest |
| 5-9 | FastHt | 2-entry hash table, increasing match length |
| 10 | Greedy | Hash chains with greedy matching |
| 11-17 | Lazy | Hash chains with lazy matching |
| 18-22 | Lazy2 | Double-lazy matching |
| 23-30 | Near-optimal | Near-optimal parsing via binary trees |

Higher effort within a strategy increases search depth and match quality.
Strategy transitions (e.g. e9→e10, e10→e11) can occasionally produce
slightly larger output on specific inputs due to algorithmic differences.
Use `CompressionLevel::monotonicity_fallback()` to detect and handle these
transitions — it returns the previous strategy's max effort so you can
compare both and pick the smaller result.

Reuse `Compressor` and `Decompressor` across calls to avoid re-initialization.

#### Recommended effort levels

Benchmarked on real images (10 screenshots, 10 photos) from the
[codec-corpus](https://crates.io/crates/codec-corpus). Ratio = compressed / raw
size (lower is better). Speed = compression throughput.

| Effort | Preset | Strategy | Screenshots | Photos | Note |
|--------|--------|----------|-------------|--------|------|
| 1 | `fastest()` | Turbo | 6.2%, 2360 MiB/s | 73.4%, 225 MiB/s | Max throughput |
| 9 | — | FastHt | 5.9%, 2175 MiB/s | 73.0%, 164 MiB/s | Best cheap compression |
| 10 | `fast()` | Greedy | 5.3%, 630 MiB/s | 70.7%, 118 MiB/s | Hash chains — big ratio jump |
| 15 | `balanced()` | Lazy | 5.1%, 466 MiB/s | 69.7%, 90 MiB/s | Good default |
| 22 | `high()` | Lazy2 | 4.9%, 197 MiB/s | 69.8%, 72 MiB/s | Best before near-optimal |
| 30 | `best()` | NearOptimal | 4.4%, 11 MiB/s | 67.4%, 19 MiB/s | Maximum compression |

For most uses, `balanced()` (effort 15) is a good default. Use `fast()` (effort 10)
when speed matters more than the last few percent of compression.

### Parallel gzip compression

```rust
use zenflate::{Compressor, CompressionLevel, Unstoppable};

let mut compressor = Compressor::new(CompressionLevel::balanced());
let bound = Compressor::gzip_compress_bound(data.len()) + num_threads * 5;
let mut compressed = vec![0u8; bound];
let size = compressor
    .gzip_compress_parallel(data, &mut compressed, 4, Unstoppable)
    .unwrap();
```

Splits input into chunks with 32KB dictionary overlap, compresses in parallel,
concatenates into a valid gzip stream. Near-linear scaling (3.3x with 4 threads).

### Cancellation

All compression and whole-buffer decompression methods accept a `stop` parameter
implementing the `Stop` trait. Pass `Unstoppable` to disable cancellation, or
implement `Stop` to check a flag periodically:

```rust
use zenflate::{Stop, StopReason, Unstoppable};

// Unstoppable — never cancels
compressor.deflate_compress(data, &mut out, Unstoppable)?;

// Custom cancellation
struct MyStop { cancelled: std::sync::Arc<std::sync::atomic::AtomicBool> }
impl Stop for MyStop {
    fn check(&self) -> Result<(), StopReason> {
        if self.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            Err(StopReason)
        } else {
            Ok(())
        }
    }
}
```

Streaming decompression doesn't take a `Stop` parameter — the caller controls
the loop and can stop between `fill()` calls.

## Features

| Feature | Default | Effect |
|---------|---------|--------|
| `std` | yes | `std::error::Error` impls, `BufReadSource`, parallel gzip |
| `alloc` | yes (via `std`) | Compression, streaming decompression |
| `avx512` | yes | AVX-512 SIMD for checksums on supported CPUs |
| `unchecked` | no | Elide bounds checks in hot paths (+10-25% compression speed) |

Decompression works in `no_std` without `alloc`; all state is stack-allocated.

## Performance

Benchmarked on x86_64 with AVX-512 (Intel), `--features unchecked`.

**Compression** (3 MiB photo bitmap, reproducible via `examples/ratio_bench.rs`):

| Library | Level | Ratio | Speed | vs C |
|---------|-------|-------|-------|------|
| **zenflate** | effort 1 (fastest) | 91.69% | 149 MiB/s | 0.81x |
| **zenflate** | effort 15 (balanced) | 92.31% | 105 MiB/s | 0.88x |
| **zenflate** | effort 22 (high) | 92.31% | 104 MiB/s | 0.87x |
| **zenflate** | effort 30 (best) | 91.80% | 39 MiB/s | 0.89x |
| libdeflate (C) | L1 | 91.69% | 185 MiB/s | — |
| libdeflate (C) | L9 | 92.31% | 119 MiB/s | — |
| libdeflate (C) | L12 | 91.80% | 44 MiB/s | — |
| flate2 | L1 | 91.70% | 291 MiB/s | — |
| flate2 | L9 (best) | 91.58% | 55 MiB/s | — |

zenflate and libdeflate produce **byte-identical output** at every level
(via `CompressionLevel::libdeflate(n)`).

**Decompression** (compressed at L6):

| Data type | zenflate | libdeflate (C) | flate2 | miniz_oxide |
|-----------|----------|----------------|--------|-------------|
| Sequential | 27.7 GiB/s | 31.6 GiB/s | 7.2 GiB/s | 6.6 GiB/s |
| Zeros | 34.6 GiB/s | 14.5 GiB/s | 26.6 GiB/s | 17.2 GiB/s |
| Mixed | 717 MiB/s | 795 MiB/s | 585 MiB/s | 571 MiB/s |

**Checksums:**

| Algorithm | zenflate | libdeflate (C) | Implementation |
|-----------|----------|----------------|----------------|
| Adler-32 | 114 GiB/s | 121 GiB/s | AVX-512 VNNI (x86), dotprod (aarch64), WASM simd128 |
| CRC-32 | 78 GiB/s | 77 GiB/s | PCLMULQDQ (x86), PMULL (aarch64) |

**Parallel gzip** (4 MB mixed data):

| Level | 1 thread | 4 threads | Speedup |
|-------|----------|-----------|---------|
| effort 1 | 161 MiB/s | 534 MiB/s | 3.3x |
| effort 15 | 133 MiB/s | 440 MiB/s | 3.3x |
| effort 30 | 46 MiB/s | 135 MiB/s | 2.9x |

## How it works

A line-by-line port of Eric Biggers' [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust (`#![forbid(unsafe_code)]` by default). Same matchfinders (hash table, hash chains, binary trees), same Huffman construction, same block splitting heuristics, same near-optimal parser.

zenflate extends libdeflate with:
- **Effort-based compression (0-30)** with additional strategies (turbo, fast HT) and finer-grained parameter tuning between libdeflate's 13 fixed levels.
- **Parallel gzip compression** using pigz-style chunking with 32KB dictionary overlap and combined CRC-32.
- **Streaming decompression** via a pull-based API that works in `no_std + alloc`.

The C original is faster — zenflate runs at roughly 0.8-0.9x the speed of libdeflate depending on level and data. The gap comes from register pressure differences and bounds checking. The `unchecked` feature closes some of this gap.

SIMD acceleration for checksums (AVX2/AVX-512/PCLMULQDQ on x86, NEON/dotprod/PMULL on aarch64, simd128 on WASM). Runtime feature detection via [archmage](https://crates.io/crates/archmage) with zero `unsafe`.

## License

MIT

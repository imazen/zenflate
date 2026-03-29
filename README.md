# zenflate [![CI](https://img.shields.io/github/actions/workflow/status/imazen/zenflate/ci.yml?branch=main&style=flat-square)](https://github.com/imazen/zenflate/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/zenflate?style=flat-square)](https://crates.io/crates/zenflate) [![lib.rs](https://img.shields.io/crates/v/zenflate?style=flat-square&label=lib.rs&color=blue)](https://lib.rs/crates/zenflate) [![docs.rs](https://img.shields.io/docsrs/zenflate?style=flat-square)](https://docs.rs/zenflate) [![license](https://img.shields.io/badge/license-AGPL--3.0%20%2F%20Commercial-blue?style=flat-square)](https://github.com/imazen/zenflate#license)

Pure Rust DEFLATE/zlib/gzip compression and decompression.

`no_std` compatible (`alloc` required for compression and streaming decompression; decompression is fully stack-allocated).

## Usage

```toml
[dependencies]
zenflate = "0.3"
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
| `unchecked` | no | Elide bounds checks in hot paths (+0-12% compression speed) |

Decompression works in `no_std` without `alloc`; all state is stack-allocated.

## Performance

Benchmarked on x86_64 with AVX-512 (Intel), `--features unchecked` (v0.3.1).
As of v0.3.2, `NearOptimalState` uses `Vec` instead of fixed arrays; benchmarks
should be re-run to confirm performance at levels 10-12 and 30.

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
| Adler-32 | 114 GiB/s | 121 GiB/s | AVX-512 VNNI (x86), NEON (aarch64), WASM simd128 |
| CRC-32 | 78 GiB/s | 77 GiB/s | PCLMULQDQ (x86), PMULL (aarch64) |

**Parallel gzip** (4 MB mixed data):

| Level | 1 thread | 4 threads | Speedup |
|-------|----------|-----------|---------|
| effort 1 | 161 MiB/s | 534 MiB/s | 3.3x |
| effort 15 | 133 MiB/s | 440 MiB/s | 3.3x |
| effort 30 | 46 MiB/s | 135 MiB/s | 2.9x |

## How it works

zenflate started as a port of Eric Biggers'
[libdeflate](https://github.com/ebiggers/libdeflate) and has grown into its
own implementation. The core decompressor, matchfinders, Huffman construction,
and block splitting trace back to libdeflate. On top of that foundation,
zenflate pulls in techniques from several other projects and adds original work:

- **Effort-based compression (0-30)** with six strategies and named presets,
  replacing libdeflate's fixed 0-12 levels. Includes two original matchfinder
  designs (turbo, fast HT) for the low-effort range.
- **Full-optimal compression** (Zopfli-style iterative squeeze), ported from
  [zenzop](https://github.com/imazen/zenzop) with Katajainen bounded
  package-merge for optimal length-limited Huffman codes.
- **Multi-strategy Huffman optimization** combining Brotli-inspired frequency
  smoothing, Zopfli-style RLE optimization, and max-bits sweeps to find the
  smallest encoding per block.
- **Parallel gzip compression** using pigz-style chunking with 32KB dictionary
  overlap and combined CRC-32 via GF(2) matrix.
- **Streaming decompression** via a pull-based API that works in `no_std + alloc`.
- **Snapshot/restore** (`CompressorSnapshot`) for branching compression state —
  try different inputs from the same point and pick the best result (designed
  for PNG filter selection).
- **Cancellation** via the `Stop` trait for cooperative interruption.

Safe Rust throughout (`#![forbid(unsafe_code)]` by default), with an opt-in
`unchecked` feature for bounds-check elimination in compression hot paths.
SIMD acceleration for checksums (AVX2/AVX-512/PCLMULQDQ on x86, NEON/PMULL on
aarch64, simd128 on WASM) via [archmage](https://crates.io/crates/archmage)
with zero `unsafe`.

zenflate can produce byte-identical output to libdeflate at every level (via
`CompressionLevel::libdeflate(n)`), and runs at roughly 0.8-0.9x the speed
of the C original depending on level and data. The gap comes from register
pressure differences and bounds checking.

### Acknowledgments

- [libdeflate](https://github.com/ebiggers/libdeflate) by Eric Biggers —
  decompressor, matchfinders (hash table, hash chains, binary trees), Huffman
  construction, block splitting, near-optimal parser, checksum implementations
- [Zopfli](https://github.com/google/zopfli) by Lode Vandevenne and
  Jyrki Rissanen (Google) — full-optimal parsing concept, iterative cost
  refinement, `optimize_huffman_for_rle` (Zopfli-style variant)
- [zenzop](https://github.com/imazen/zenzop) — Rust Zopfli port used as the
  source for katajainen, squeeze, and block splitter modules
- [Brotli](https://github.com/google/brotli) (Google) — frequency smoothing
  algorithm for Huffman RLE encoding
- [pigz](https://zlib.net/pigz/) by Mark Adler — parallel gzip chunking
  strategy with dictionary overlap

### What's different from libdeflate

`CompressionLevel::libdeflate(n)` produces byte-identical output to C. The
recommended effort-based API (`CompressionLevel::new(n)`) uses different
algorithms and tuning at every level:

| Effort | Strategy | Matchfinder | Encoding | vs libdeflate |
|--------|----------|-------------|----------|---------------|
| 0 | Store | — | — | Same |
| 1-4 | Turbo | Single-entry hash, limited skip updates | Standard | **Original** matchfinder, not in libdeflate |
| 5-9 | FastHt | 2-entry hash, limited skip updates | Standard | **Original** matchfinder, not in libdeflate |
| 10 | Greedy | Hash chains | Standard | `good_match` early-exit (libdeflate: disabled) |
| 11-17 | Lazy | Hash chains | Standard | `good_match`/`max_lazy` tuning curves (libdeflate: disabled) |
| 18-22 | Lazy2 | Hash chains | Standard | `good_match`/`max_lazy` tuning (libdeflate: disabled) |
| 23-25 | NearOptimal | Binary trees | Exhaustive precode search | Multi-strategy precode flag search |
| 26-27 | NearOptimal | Binary trees | + multi-strategy Huffman | + Brotli/Zopfli RLE smoothing, reduced max_bits sweep |
| 28-30 | NearOptimal | Binary trees | + diversified optimization | + randomized cost model, 20-30 passes (libdeflate: 2-10) |
| 31+ | FullOptimal | Zopfli hash chains | Katajainen package-merge | **Entirely different** algorithm (from zenzop) |

At effort 10-22, the core matching algorithms are the same as libdeflate
(greedy, lazy, double-lazy with hash chains), but zenflate adds `good_match`
and `max_lazy` early-exit thresholds that libdeflate leaves disabled. These
let the compressor skip deep chain searches and lazy evaluations when it
already has a good enough match, trading a small amount of compression ratio
for speed at lower effort levels.

At effort 23+, the near-optimal parser is the same backward DP as
libdeflate, but the block encoding pipeline diverges: multi-strategy
Huffman code construction tries Brotli-inspired and Zopfli-style frequency
smoothing with max-bits sweeps to find smaller encodings. At effort 28+,
the optimizer runs 20-30 passes with randomized cost diversification
instead of libdeflate's fixed 2-10 passes.

## MSRV

The minimum supported Rust version is **1.89**.

## AI-Generated Code Notice

Developed with Claude (Anthropic). Not all code manually reviewed. Review critical paths before production use.

## Image tech I maintain

| | |
|:--|:--|
| State of the art codecs<sup>[1]</sup> | [zenjpeg] · [zenpng] · [zenwebp] · [zengif] · [zenavif] ([rav1d-safe] · [zenrav1e] · [zenavif-parse] · [zenavif-serialize]) · [zenjxl] ([jxl-encoder] · [zenjxl-decoder]) · [zentiff] · [zenbitmaps] · [heic] · [zenraw] · [zenpdf] · [ultrahdr] · [mozjpeg-rs] · [webpx] |
| Compression | **zenflate** · [zenzop] |
| Processing | [zenresize] · [zenfilters] · [zenquant] · [zenblend] |
| Metrics | [zensim] · [fast-ssim2] · [butteraugli] · [resamplescope-rs] · [codec-eval] · [codec-corpus] |
| Pixel types & color | [zenpixels] · [zenpixels-convert] · [linear-srgb] · [garb] |
| Pipeline | [zenpipe] · [zencodec] · [zencodecs] · [zenlayout] · [zennode] |
| ImageResizer | [ImageResizer] (C#) — 24M+ NuGet downloads across all packages |
| [Imageflow][] | Image optimization engine (Rust) — [.NET][imageflow-dotnet] · [node][imageflow-node] · [go][imageflow-go] — 9M+ NuGet downloads across all packages |
| [Imageflow Server][] | [The fast, safe image server](https://www.imazen.io/) (Rust+C#) — 552K+ NuGet downloads, deployed by Fortune 500s and major brands |

<sup>[1]</sup> <sub>as of 2026</sub>

### General Rust awesomeness

[archmage] · [magetypes] · [enough] · [whereat] · [zenbench] · [cargo-copter]

[And other projects](https://www.imazen.io/open-source) · [GitHub @imazen](https://github.com/imazen) · [GitHub @lilith](https://github.com/lilith) · [lib.rs/~lilith](https://lib.rs/~lilith) · [NuGet](https://www.nuget.org/profiles/imazen) (over 30 million downloads / 87 packages)

## License

Dual-licensed: [AGPL-3.0](LICENSE-AGPL3) or [commercial](LICENSE-COMMERCIAL).

I've maintained and developed open-source image server software — and the 40+
library ecosystem it depends on — full-time since 2011. Fifteen years of
continual maintenance, backwards compatibility, support, and the (very rare)
security patch. That kind of stability requires sustainable funding, and
dual-licensing is how we make it work without venture capital or rug-pulls.
Support sustainable and secure software; swap patch tuesday for patch leap-year.

[Our open-source products](https://www.imazen.io/open-source)

**Your options:**

- **Startup license** — $1 if your company has under $1M revenue and fewer
  than 5 employees. [Get a key →](https://www.imazen.io/pricing)
- **Commercial subscription** — Governed by the Imazen Site-wide Subscription
  License v1.1 or later. Apache 2.0-like terms, no source-sharing requirement.
  Sliding scale by company size.
  [Pricing & 60-day free trial →](https://www.imazen.io/pricing)
- **AGPL v3** — Free and open. Share your source if you distribute.

See [LICENSE-COMMERCIAL](LICENSE-COMMERCIAL) for details.

Upstream code from [ebiggers/libdeflate](https://github.com/ebiggers/libdeflate) is licensed under MIT.
Our additions and improvements are dual-licensed (AGPL-3.0 or commercial) as above.

### Upstream Contribution

We are willing to release our improvements under the original MIT
license if upstream takes over maintenance of those improvements. We'd rather
contribute back than maintain a parallel codebase. Open an issue or reach out.

[zenjpeg]: https://github.com/imazen/zenjpeg
[zenpng]: https://github.com/imazen/zenpng
[zenwebp]: https://github.com/imazen/zenwebp
[zengif]: https://github.com/imazen/zengif
[zenavif]: https://github.com/imazen/zenavif
[zenjxl]: https://github.com/imazen/zenjxl
[zentiff]: https://github.com/imazen/zentiff
[zenbitmaps]: https://github.com/imazen/zenbitmaps
[heic]: https://github.com/imazen/heic-decoder-rs
[zenraw]: https://github.com/imazen/zenraw
[zenpdf]: https://github.com/imazen/zenpdf
[ultrahdr]: https://github.com/imazen/ultrahdr
[jxl-encoder]: https://github.com/imazen/jxl-encoder
[zenjxl-decoder]: https://github.com/imazen/zenjxl-decoder
[rav1d-safe]: https://github.com/imazen/rav1d-safe
[zenrav1e]: https://github.com/imazen/zenrav1e
[mozjpeg-rs]: https://github.com/imazen/mozjpeg-rs
[zenavif-parse]: https://github.com/imazen/zenavif-parse
[zenavif-serialize]: https://github.com/imazen/zenavif-serialize
[webpx]: https://github.com/imazen/webpx
[zenzop]: https://github.com/imazen/zenzop
[zenresize]: https://github.com/imazen/zenresize
[zenfilters]: https://github.com/imazen/zenfilters
[zenquant]: https://github.com/imazen/zenquant
[zenblend]: https://github.com/imazen/zenblend
[zensim]: https://github.com/imazen/zensim
[fast-ssim2]: https://github.com/imazen/fast-ssim2
[butteraugli]: https://github.com/imazen/butteraugli
[zenpixels]: https://github.com/imazen/zenpixels
[zenpixels-convert]: https://github.com/imazen/zenpixels
[linear-srgb]: https://github.com/imazen/linear-srgb
[garb]: https://github.com/imazen/garb
[zenpipe]: https://github.com/imazen/zenpipe
[zencodec]: https://github.com/imazen/zencodec
[zencodecs]: https://github.com/imazen/zencodecs
[zenlayout]: https://github.com/imazen/zenlayout
[zennode]: https://github.com/imazen/zennode
[Imageflow]: https://github.com/imazen/imageflow
[Imageflow Server]: https://github.com/imazen/imageflow-server
[imageflow-dotnet]: https://github.com/imazen/imageflow-dotnet
[imageflow-node]: https://github.com/imazen/imageflow-node
[imageflow-go]: https://github.com/imazen/imageflow-go
[ImageResizer]: https://github.com/imazen/resizer
[archmage]: https://github.com/imazen/archmage
[magetypes]: https://github.com/imazen/archmage
[enough]: https://github.com/imazen/enough
[whereat]: https://github.com/lilith/whereat
[zenbench]: https://github.com/imazen/zenbench
[cargo-copter]: https://github.com/imazen/cargo-copter
[resamplescope-rs]: https://github.com/imazen/resamplescope-rs
[codec-eval]: https://github.com/imazen/codec-eval
[codec-corpus]: https://github.com/imazen/codec-corpus

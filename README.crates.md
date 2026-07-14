<!-- GENERATED FROM README.md by zenutils gen-readme-crates.sh — DO NOT EDIT. -->

# zenflate

Pure Rust DEFLATE / zlib / gzip. Compression spans effort levels 0–200 across seven strategies (and can emit byte-identical output to C libdeflate on demand), with whole-buffer and streaming decompression plus SIMD Adler-32 / CRC-32. `#![forbid(unsafe_code)]` by default (with an opt-in `unchecked` fast path) and `no_std`-friendly: compression and streaming decompression require `alloc`, while whole-buffer decompression is fully stack-allocated.

## Quick start

```toml
[dependencies]
zenflate = "0.4"
```

```rust
use zenflate::{Compressor, Decompressor, CompressionLevel, Unstoppable};

let data = b"the quick brown fox jumps over the lazy dog, again and again";

// Compress with the balanced preset (raw DEFLATE; zlib_/gzip_ variants share this shape).
let mut compressor = Compressor::new(CompressionLevel::balanced());
let mut packed = vec![0u8; Compressor::deflate_compress_bound(data.len())];
let n = compressor.deflate_compress(data, &mut packed, Unstoppable).unwrap();

// Decompress into a caller-sized buffer — its length is your hard size cap.
let mut out = vec![0u8; data.len()];
let r = Decompressor::new()
    .deflate_decompress(&packed[..n], &mut out, Unstoppable)
    .unwrap();
assert_eq!(&out[..r.output_written], data);
```

Need gzip/zlib framing, streaming, parallel gzip, cancellation, or fine-grained
effort control? Those are covered below.

## Usage

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

For gzip and zlib, use `gzip_decompress` / `zlib_decompress` (identical shape).

**Server safety — bound the output.** The one-shot decompressors write into the
`&mut [u8]` you pass, so **that buffer is the size cap**: for untrusted input you
don't know the decompressed length up front (the gzip trailer is attacker-
controlled), so size `output` to your maximum and decompression returns an error
rather than over-allocating. If you instead use the streaming
[`StreamDecompressor`](#streaming-decompression) (which grows its own buffer),
cap it explicitly with `.with_max_output_size(Some(max_bytes))` — otherwise a
small "zip bomb" can expand without bound.

```rust
// gzip into a hard-capped buffer (rejects anything larger):
let mut out = vec![0u8; 100 * 1024 * 1024]; // 100 MiB ceiling
match Decompressor::new().gzip_decompress(gzip_bytes, &mut out, Unstoppable) {
    Ok(r) => { /* r.output_written bytes are valid */ }
    Err(e) => { /* malformed input or output exceeds the 100 MiB ceiling */ }
}
```

### Streaming decompression

For inputs that don't fit in memory or arrive incrementally. Works with
`&[u8]` (zero overhead) or any `std::io::BufRead` via `BufReadSource`.

Construct with `deflate`/`zlib`/`gzip` (each takes the source plus an output
buffer capacity — `DEFAULT_CAPACITY` is 64 KiB), then drive the
`fill` → `peek` → `advance` loop until `is_done()`:

```rust
use zenflate::{StreamDecompressor, DEFAULT_CAPACITY};

// From a slice (`&[u8]` is a zero-overhead source):
let mut stream = StreamDecompressor::deflate(compressed_data, DEFAULT_CAPACITY);
while !stream.is_done() {
    stream.fill()?;             // pull from source, decompress into the buffer
    let chunk = stream.peek();  // borrow the available decompressed output
    // process chunk...
    let n = chunk.len();
    stream.advance(n);          // mark consumed, freeing buffer space
}

// From a BufRead (std only):
use zenflate::BufReadSource;
let file = std::io::BufReader::new(std::fs::File::open("data.gz").unwrap());
let mut stream = StreamDecompressor::gzip(BufReadSource::new(file), DEFAULT_CAPACITY);
// stream also implements Read + BufRead
```

**Untrusted input / decompression bombs.** The whole-buffer `Decompressor`
is naturally bounded by the output slice you pass it. The streaming API
produces output incrementally, so for untrusted data cap the total with
`with_max_output_size` (decoding then errors instead of allocating past the
cap); a stall guard also rejects streams that emit thousands of empty blocks
without progress:

```rust
let mut stream = StreamDecompressor::gzip(compressed_data, DEFAULT_CAPACITY)
    .with_max_output_size(Some(64 * 1024 * 1024)); // DecompressionError::OutputLimitExceeded past 64 MiB
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

Pick a preset or dial in a specific effort from 0 to 200:

```rust
use zenflate::CompressionLevel;

// Named presets
CompressionLevel::none()      // effort 0  — store (no compression)
CompressionLevel::fastest()   // effort 1  — turbo hash table
CompressionLevel::fast()      // effort 10 — greedy hash chains
CompressionLevel::balanced()  // effort 15 — lazy matching (default)
CompressionLevel::high()      // effort 22 — double-lazy matching
CompressionLevel::best()      // effort 30 — near-optimal parsing

// Fine-grained control (0-200, clamped)
CompressionLevel::new(12)     // lazy matching, mid-range
CompressionLevel::new(25)     // near-optimal, fast end
CompressionLevel::new(46)     // Zopfli-style full-optimal, 30 iterations

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

Effort levels map to seven strategies:

| Effort | Strategy | Notes |
|--------|----------|-------|
| 0 | Store | No compression |
| 1-4 | Turbo | Single-entry hash table, fastest |
| 5-9 | FastHt | 2-entry hash table, increasing match length |
| 10 | Greedy | Hash chains with greedy matching |
| 11-17 | Lazy | Hash chains with lazy matching |
| 18-22 | Lazy2 | Double-lazy matching |
| 23-30 | Near-optimal | Near-optimal parsing via binary trees |
| 31-200 | FullOptimal | Zopfli-style iterative optimal parsing (`iterations = effort − 16`); very slow, maximum density |

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
| `std` | yes | `std::error::Error` impls, `BufReadSource` |
| `alloc` | yes (via `std`) | Streaming decompression |
| `compress` | yes | `Compressor` / `CompressionLevel` (implies `alloc`) |
| `simd` | yes | Runtime-dispatched SIMD checksums and matchfinder multiversioning (via archmage); without it, scalar paths |
| `avx512` | yes | AVX-512 SIMD tiers (implies `simd`) |
| `threads` | yes | Parallel gzip (`gzip_compress_parallel`, implies `compress`); disable for thread-less `wasm32` |
| `unchecked` | no | Elide bounds checks in compression hot paths (+0-12% compression speed) |

Decompression works in `no_std` without `alloc`; all state is stack-allocated.

For a minimal, fast-to-compile decoder, disable default features:

```toml
zenflate = { version = "0.4.0", default-features = false, features = ["std"] }
```

That decode-only configuration builds in well under a second with a single
dependency (`enough`) — no proc macros, no SIMD — and still decodes all three
formats with checksum verification (scalar Adler-32/CRC-32).

**Migrating from 0.3:** with `default-features = false`, add `compress` if you
compress and `simd` if you want SIMD checksums; both were previously implied
by `alloc` / always-on.

## How it works

zenflate started as a port of Eric Biggers'
[libdeflate](https://github.com/ebiggers/libdeflate) and has grown into its
own implementation. The core decompressor, matchfinders, Huffman construction,
and block splitting trace back to libdeflate. On top of that foundation,
zenflate pulls in techniques from several other projects and adds original work:

- **Effort-based compression (0-200)** with seven strategies and named presets,
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

## License

Dual-licensed: [AGPL-3.0](https://github.com/imazen/zenflate/blob/main/LICENSE-AGPL3) or [commercial](https://github.com/imazen/zenflate/blob/main/LICENSE-COMMERCIAL).

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

See [LICENSE-COMMERCIAL](https://github.com/imazen/zenflate/blob/main/LICENSE-COMMERCIAL) for details.

Upstream code from [ebiggers/libdeflate](https://github.com/ebiggers/libdeflate) is licensed under MIT.
Our additions and improvements are dual-licensed (AGPL-3.0 or commercial) as above.

## Image tech I maintain

| | |
|:--|:--|
| **Codecs** ¹ | [zenjpeg] · [zenpng] · [zenwebp] · [zengif] · [zenavif] · [zenjxl] · [zenbitmaps] · [heic] · [zentiff] · [zenpdf] · [zensvg] · [zenjp2] · [zenraw] · [ultrahdr] |
| Codec internals | [zenjxl-decoder] · [jxl-encoder] · [zenrav1e] · [rav1d-safe] · [zenavif-parse] · [zenavif-serialize] |
| Compression | **zenflate** · [zenzop] · [zenzstd] |
| Processing | [zenresize] · [zenquant] · [zenblend] · [zenfilters] · [zensally] · [zentone] |
| Pixels & color | [zenpixels] · [zenpixels-convert] · [linear-srgb] · [garb] |
| Pipeline & framework | [zenpipe] · [zencodec] · [zencodecs] · [zenlayout] · [zennode] · [zenwasm] · [zentract] |
| Metrics | [zensim] · [fast-ssim2] · [butteraugli] · [zenmetrics] · [resamplescope-rs] |
| Pickers & ML | [zenanalyze] · [zenpredict] · [zenpicker] |
| Products | [Imageflow] image engine ([.NET][imageflow-dotnet] · [Node][imageflow-node] · [Go][imageflow-go]) · [Imageflow Server] · [ImageResizer] (C#) |

<sub>¹ pure-Rust, `#![forbid(unsafe_code)]` codecs, as of 2026</sub>

### General Rust awesomeness

[zenbench] · [archmage] · [magetypes] · [enough] · [whereat] · [cargo-copter]

[Open source](https://www.imazen.io/open-source) · [@imazen](https://github.com/imazen) · [@lilith](https://github.com/lilith) · [lib.rs/~lilith](https://lib.rs/~lilith)

[zenjpeg]: https://github.com/imazen/zenjpeg
[zenpng]: https://github.com/imazen/zenpng
[zenwebp]: https://github.com/imazen/zenwebp
[zengif]: https://github.com/imazen/zengif
[zenavif]: https://github.com/imazen/zenavif
[zenjxl]: https://github.com/imazen/zenjxl
[zenbitmaps]: https://github.com/imazen/zenbitmaps
[heic]: https://github.com/imazen/heic
[zentiff]: https://github.com/imazen/zentiff
[zenpdf]: https://github.com/imazen/zenpdf
[zensvg]: https://github.com/imazen/zenextras
[zenjp2]: https://github.com/imazen/zenextras
[zenraw]: https://github.com/imazen/zenraw
[ultrahdr]: https://github.com/imazen/ultrahdr
[zenjxl-decoder]: https://github.com/imazen/zenjxl-decoder
[jxl-encoder]: https://github.com/imazen/jxl-encoder
[zenrav1e]: https://github.com/imazen/zenrav1e
[rav1d-safe]: https://github.com/imazen/rav1d-safe
[zenavif-parse]: https://github.com/imazen/zenavif-parse
[zenavif-serialize]: https://github.com/imazen/zenavif-serialize
[zenzop]: https://github.com/imazen/zenzop
[zenzstd]: https://github.com/imazen/zenzstd
[zenresize]: https://github.com/imazen/zenresize
[zenquant]: https://github.com/imazen/zenquant
[zenblend]: https://github.com/imazen/zenblend
[zenfilters]: https://github.com/imazen/zenfilters
[zensally]: https://github.com/imazen/zensally
[zentone]: https://github.com/imazen/zentone
[zenpixels]: https://github.com/imazen/zenpixels
[zenpixels-convert]: https://github.com/imazen/zenpixels
[linear-srgb]: https://github.com/imazen/linear-srgb
[garb]: https://github.com/imazen/garb
[zenpipe]: https://github.com/imazen/zenpipe
[zencodec]: https://github.com/imazen/zencodec
[zencodecs]: https://github.com/imazen/zencodecs
[zenlayout]: https://github.com/imazen/zenlayout
[zennode]: https://github.com/imazen/zennode
[zenwasm]: https://github.com/imazen/zenwasm
[zentract]: https://github.com/imazen/zentract
[zensim]: https://github.com/imazen/zensim
[fast-ssim2]: https://github.com/imazen/fast-ssim2
[butteraugli]: https://github.com/imazen/butteraugli
[zenmetrics]: https://github.com/imazen/zenmetrics
[resamplescope-rs]: https://github.com/imazen/resamplescope-rs
[zenanalyze]: https://github.com/imazen/zenanalyze
[zenpredict]: https://github.com/imazen/zenanalyze
[zenpicker]: https://github.com/imazen/zenanalyze
[zenbench]: https://github.com/imazen/zenbench
[archmage]: https://github.com/imazen/archmage
[magetypes]: https://github.com/imazen/archmage
[enough]: https://github.com/imazen/enough
[whereat]: https://github.com/lilith/whereat
[cargo-copter]: https://github.com/imazen/cargo-copter
[Imageflow]: https://github.com/imazen/imageflow
[Imageflow Server]: https://github.com/imazen/imageflow-dotnet-server
[ImageResizer]: https://github.com/imazen/resizer
[imageflow-dotnet]: https://github.com/imazen/imageflow-dotnet
[imageflow-node]: https://github.com/imazen/imageflow-node
[imageflow-go]: https://github.com/imazen/imageflow-go

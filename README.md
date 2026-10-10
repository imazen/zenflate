# zenflate [![CI](https://img.shields.io/github/actions/workflow/status/imazen/zenflate/ci.yml?style=flat-square&label=CI)](https://github.com/imazen/zenflate/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/zenflate?style=flat-square)](https://crates.io/crates/zenflate) [![lib.rs](https://img.shields.io/crates/v/zenflate?style=flat-square&label=lib.rs&color=blue)](https://lib.rs/crates/zenflate) [![docs.rs](https://img.shields.io/docsrs/zenflate?style=flat-square)](https://docs.rs/zenflate) [![license](https://img.shields.io/badge/license-AGPL--3.0%20%2F%20Commercial-blue?style=flat-square)](#license) [![MSRV](https://img.shields.io/badge/MSRV-1.89-blue?style=flat-square)](https://doc.rust-lang.org/cargo/reference/manifest.html#the-rust-version-field)

Pure Rust DEFLATE / zlib / gzip. Compression spans effort levels 0–200 (with a separate C libdeflate compatibility mode), with whole-buffer and streaming decompression plus SIMD Adler-32 / CRC-32. `#![forbid(unsafe_code)]` by default (with an opt-in `unchecked` fast path) and `no_std`-friendly: compression and streaming decompression require `alloc`, while whole-buffer decompression works without `alloc`. With `std`, the fixed-Huffman table cache allocates once per process.

## Quick start

```toml
[dependencies]
zenflate = "0.4.1"
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

The one-shot decoders may overwrite up to about 32 bytes of `output` past
`output_written` when the buffer is larger than the decoded data (the fast
loop copies matches in fixed-size chunks). Bytes past `output_written` are not
preserved, so don't decode into part of a buffer whose tail you need.

**Server safety — bound the output.** The one-shot decompressors write into the
`&mut [u8]` you pass, so **that buffer is the size cap**: for untrusted input you
don't know the decompressed length up front (the gzip trailer is attacker-
controlled), so size `output` to your maximum and decompression returns an error
rather than over-allocating. If you instead use the streaming
[`StreamDecompressor`](#streaming-decompression) (which manages its own buffer),
limit total decoded output with `.with_max_output_size(Some(max_bytes))`.

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
`&[u8]` or any `std::io::BufRead` via `BufReadSource`.

Construct with `deflate`/`zlib`/`gzip` (each takes the source plus an output
buffer capacity — `DEFAULT_CAPACITY` is 64 KiB), then drive the
`fill` → `peek` → `advance` loop until `is_done()`:

```rust
use zenflate::{StreamDecompressor, DEFAULT_CAPACITY};

// From a slice (`&[u8]` is an input source):
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
`with_max_output_size` (decoding errors when total output would exceed the
cap); a stall guard also rejects streams that emit thousands of empty blocks
without progress:

```rust
let mut stream = StreamDecompressor::gzip(compressed_data, DEFAULT_CAPACITY)
    .with_max_output_size(Some(64 * 1024 * 1024)); // DecompressionError::OutputLimitExceeded past 64 MiB
```

### Checksum policy

Zlib and gzip decoding verify checksums by default. Both decoder types accept
`with_checksum(policy)`, with `ChecksumPolicy::Verify`, `Report` or `Ignore`.
`Report` computes the
checksum and exposes the comparison through `checksum_matched()` without
rejecting a mismatch. `Ignore` avoids checksum computation and comparison;
gzip's uncompressed-length check still applies. `with_skip_checksum(true)`
retains its existing meaning: `Report`.

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
| `fastest()` | 1 | Turbo | Low-effort preset |
| `fast()` | 10 | Greedy | Hash chains |
| `balanced()` | 15 | Lazy | Lazy matching — good default |
| `high()` | 22 | Lazy2 | Double-lazy matching |
| `best()` | 30 | Near-optimal | Near-optimal preset |

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

For most uses, `balanced()` (effort 15) is a good default. Use `fast()` (effort 10)
to spend less time searching for matches.

### PNG image data

`CompressionLevel::png(effort)` is tuned for PNG IDAT streams: filtered
scanlines with long byte runs and literal-heavy residuals.

| Effort | Encoder |
|--------|---------|
| `png(1)` | Ultra-fast: literals and zero runs, one Huffman table per stream |
| `png(2)` | Runs only, exact Huffman tables per block |
| `png(3)` | Runs + hashed repeats of 8+ bytes |
| `png(4..=9)` | Runs + hashed repeats of 5+ bytes, hash chains of growing depth |
| `png(10..=18)` | Lazy matching, search depth 16 → 800 |
| `png(19..=22)` | Near-optimal parsing, search depth 16 → 35 |
| `png(23..=30)` | `new(23..=30)`'s near-optimal settings |
| `png(31..)` | Same as `new(31..)` (full optimal parsing) |

From `png(3)` through `png(30)` a runs-only guard compares the selected blocks
against a runs-only parse and writes the cheaper Huffman encoding. After a
clear loss, the guard skips the next three blocks. This improves flat-colour
art but does not guarantee output no larger than `png(2)`.
From `png(10)` through `png(26)` target block boundaries come from the input
alone; near-optimal parsing can still end a block early if its match cache
fills. `png(27..=30)` can also split inside those input-derived segments;
higher effort does not guarantee smaller output. `monotonicity_fallback()` names the lower level
to compare against at each change of algorithm.

```rust
use zenflate::{Compressor, CompressionLevel, Unstoppable};

let mut compressor = Compressor::new(CompressionLevel::png(6));
let mut idat = vec![0u8; Compressor::zlib_compress_bound(filtered_rows.len())];
let size = compressor.zlib_compress(&filtered_rows, &mut idat, Unstoppable)?;
```

### PNG strips for parallel encode and decode

`zenflate::png::{StripCompressor, StripDecoder}` split one PNG zlib stream into
independent strips of whole rows (PNG's `iDOT` layout). Each strip is
compressed without history from earlier strips, so strips can be compressed
on separate threads, and the concatenation is still one valid zlib stream that
any decoder reads. `StripDecoder` inflates one strip on its own (streaming,
`fill`/`peek`/`advance`) and reports whether it ended where the next strip
begins, so an `iDOT`-aware decoder can inflate strips in parallel and verify
the result against the stream's Adler-32.

```rust
use zenflate::png::StripCompressor;
use zenflate::{CompressionLevel, Unstoppable, adler32, adler32_combine};

let mut c = StripCompressor::new(CompressionLevel::png(4));
let mut z = c.zlib_header().to_vec();
let mut adler = 1;
for (k, strip) in strips.iter().enumerate() {
    let mut out = vec![0u8; StripCompressor::bound(strip.len())];
    let n = c.compress(strip, k + 1 == strips.len(), &mut out, Unstoppable)?;
    z.extend_from_slice(&out[..n]);
    adler = adler32_combine(adler, adler32(1, strip), strip.len());
}
z.extend_from_slice(&adler.to_be_bytes());
```

For ordinary sequential decoding, `compress_with_history(input, strip_start,
is_last, output, stop)` lets a strip reference the preceding 32 KiB. Supply the
history and strip in one slice; only `input[strip_start..]` is emitted. Those
strips can be compressed independently but must be decoded in order with the
preceding output available. Do not use them for independent iDOT decoding.
Efforts above 30 currently ignore history and compress the strip alone.

### Parallel gzip compression

```rust
use zenflate::{Compressor, CompressionLevel, Unstoppable};

let mut compressor = Compressor::new(CompressionLevel::balanced());
let num_threads = 4;
let bound = Compressor::gzip_compress_bound(data.len()) + num_threads * 5;
let mut compressed = vec![0u8; bound];
let size = compressor
    .gzip_compress_parallel(data, &mut compressed, num_threads, Unstoppable)
    .unwrap();
```

Splits input into chunks with 32KB dictionary overlap, compresses in parallel,
concatenates into a valid gzip stream. Scaling depends on input size and content.

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
            Err(StopReason::Cancelled)
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
| `std` | yes | `std::io::{Read, BufRead}` integration (`BufReadSource`) |
| `alloc` | yes (via `std`) | Streaming decompression |
| `compress` | yes | `Compressor` / `CompressionLevel` (implies `alloc`) |
| `simd` | yes | Runtime-dispatched SIMD checksums and matchfinder multiversioning (via archmage); without it, scalar paths |
| `avx512` | yes | AVX-512 SIMD tiers (implies `simd`) |
| `threads` | yes | Parallel gzip (`gzip_compress_parallel`, implies `compress`); disable for thread-less `wasm32` |
| `unchecked` | no | Elide bounds checks in compression hot paths |

Decompression works in `no_std` without `alloc`; all state is stack-allocated.

For a minimal, fast-to-compile decoder, disable default features:

```toml
zenflate = { version = "0.4.1", default-features = false, features = ["std"] }
```

That decode-only configuration has a single direct
dependency (`enough`) — no proc macros, no SIMD — and still decodes all three
formats with checksum verification (scalar Adler-32/CRC-32).

**Migrating from 0.3:** with `default-features = false`, add `compress` if you
compress and `simd` if you want SIMD checksums; both were previously implied
by `alloc` / always-on.

## Performance

Performance depends on input, effort, CPU and enabled features. This release
makes no speed or compression-ratio comparison claims. Dated measurements
and their commands remain in [benchmarks/](https://github.com/imazen/zenflate/tree/main/benchmarks);
consult each record for its commit and workload. A refreshed comparison suite
is planned for a later release.

## How it works

zenflate started as a port of Eric Biggers'
[libdeflate](https://github.com/ebiggers/libdeflate) and has grown into its
own implementation. The core decompressor, matchfinders, Huffman construction,
and block splitting trace back to libdeflate. On top of that foundation,
zenflate pulls in techniques from several other projects and adds original work:

- **Effort-based compression (0-200)** with named presets,
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
- [fdeflate](https://github.com/image-rs/fdeflate) (image-rs) — the PNG
  ultra-fast, runs-only and greedy compressors that `png(1..=9)` adapt, and
  the double-literal decode tables and chunked match copy in the inflate loop

## MSRV

The minimum supported Rust version is **1.89**.

## AI-Generated Code Notice

Developed with assistance from Claude (Anthropic) and Codex (OpenAI). Not all code manually reviewed. Review critical paths before production use.

## License

Dual-licensed: [AGPL-3.0](https://github.com/imazen/zenflate/blob/main/LICENSE-AGPL3) or [commercial](https://github.com/imazen/zenflate/blob/main/LICENSE-COMMERCIAL).

See the license files for the applicable terms. Commercial licensing information
is available at [Imazen](https://www.imazen.io/pricing).

Upstream code from [ebiggers/libdeflate](https://github.com/ebiggers/libdeflate) is licensed under MIT.
Our additions and improvements are dual-licensed (AGPL-3.0 or commercial) as above.

## Image tech I maintain

| | |
|:--|:--|
| **Codecs** ¹ | [zenjpeg] · [zenpng] · [zenwebp] · [zengif] · [zenavif] · [zenjxl] · [zenjxl-decoder] · [jxl-encoder] · [zenbitmaps] · [heic] · [zentiff] · [zenpdf] · [zensvg] · [zenjp2] · [zenraw] · [ultrahdr] |
| Codec internals | [zenrav1e] · [rav1d-safe] · [zenravif] · [zenavif-parse] · [zenavif-serialize] |
| Compression | **zenflate** · [zenzop] · [zenzstd] |
| Processing | [zenresize] · [zenquant] · [zenblend] · [zenfilters] · [zensally] · [zentone] |
| Pixels & color | [zenpixels] · [zenpixels-convert] · [linear-srgb] · [garb] · [zenyuv] |
| Pipeline & framework | [zenpipe] · [zencodec] · [zencodecs] · [zenlayout] · [zennode] · [zenwasm] · [zentract] |
| Metrics | [zensim] · [fast-ssim2] · [butteraugli] · [zenmetrics] · [resamplescope-rs] |
| Pickers & ML | [zenanalyze] · [zenpredict] · [zenpicker] · [zenanalyze-api] |
| Test corpora | [codec-corpus] · [imazen-26] |
| Products | [Imageflow] image engine ([.NET][imageflow-dotnet] · [Node][imageflow-node] · [Go][imageflow-go]) · [Imageflow Server] · [ImageResizer] (C#) |

<sub>¹ pure-Rust, `#![forbid(unsafe_code)]` codecs, as of 2026</sub>

### General Rust awesomeness

[zenbench] · [archmage] · [magetypes] · [enough] · [whereat] · [cargo-copter] · [zenutils]

[Open source](https://www.imazen.io/open-source) · [@imazen](https://github.com/imazen) · [@lilith](https://github.com/lilith) · [lib.rs/~lilith](https://lib.rs/~lilith)

[zenjpeg]: https://github.com/imazen/zenjpeg
[zenpng]: https://github.com/imazen/zenpng
[zenwebp]: https://github.com/imazen/zenwebp
[zengif]: https://github.com/imazen/zengif
[zenavif]: https://github.com/imazen/zenavif
[zenjxl]: https://github.com/imazen/zenjxl
[zenjxl-decoder]: https://github.com/imazen/zenjxl-decoder
[jxl-encoder]: https://github.com/imazen/jxl-encoder
[zenbitmaps]: https://github.com/imazen/zenbitmaps
[heic]: https://github.com/imazen/heic
[zentiff]: https://github.com/imazen/zenextras
[zenpdf]: https://github.com/imazen/zenextras
[zensvg]: https://github.com/imazen/zenextras
[zenjp2]: https://github.com/imazen/zenextras
[zenraw]: https://github.com/imazen/zenraw
[ultrahdr]: https://github.com/imazen/ultrahdr
[zenrav1e]: https://github.com/imazen/zenrav1e
[rav1d-safe]: https://github.com/imazen/rav1d-safe
[zenravif]: https://github.com/imazen/cavif-rs
[zenavif-parse]: https://github.com/imazen/zenavif
[zenavif-serialize]: https://github.com/imazen/zenavif
[zenzop]: https://github.com/imazen/zenzop
[zenzstd]: https://github.com/imazen/zenzstd
[zenresize]: https://github.com/imazen/zenresize
[zenquant]: https://github.com/imazen/zenquant
[zenblend]: https://github.com/imazen/zenblend
[zenfilters]: https://github.com/imazen/zenpipe
[zensally]: https://github.com/imazen/zensally
[zentone]: https://github.com/imazen/zentone
[zenpixels]: https://github.com/imazen/zenpixels
[zenpixels-convert]: https://github.com/imazen/zenpixels
[linear-srgb]: https://github.com/imazen/linear-srgb
[garb]: https://github.com/imazen/garb
[zenyuv]: https://github.com/imazen/zenjpeg
[zenpipe]: https://github.com/imazen/zenpipe
[zencodec]: https://github.com/imazen/zencodec
[zencodecs]: https://github.com/imazen/zenpipe
[zenlayout]: https://github.com/imazen/zenpipe
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
[zenanalyze-api]: https://github.com/imazen/zenanalyze
[codec-corpus]: https://github.com/imazen/codec-corpus
[imazen-26]: https://github.com/imazen/imazen-26
[zenbench]: https://github.com/imazen/zenbench
[archmage]: https://github.com/imazen/archmage
[magetypes]: https://github.com/imazen/archmage
[enough]: https://github.com/imazen/enough
[whereat]: https://github.com/lilith/whereat
[cargo-copter]: https://github.com/imazen/cargo-copter
[zenutils]: https://github.com/imazen/zenutils
[Imageflow]: https://github.com/imazen/imageflow
[Imageflow Server]: https://github.com/imazen/imageflow-dotnet-server
[ImageResizer]: https://github.com/imazen/resizer
[imageflow-dotnet]: https://github.com/imazen/imageflow-dotnet
[imageflow-node]: https://github.com/imazen/imageflow-node
[imageflow-go]: https://github.com/imazen/imageflow-go

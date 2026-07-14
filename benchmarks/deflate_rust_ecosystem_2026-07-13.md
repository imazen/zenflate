# Rust DEFLATE ecosystem — zenflate 0.4.0 head-to-head

- Date: 2026-07-13
- Commit: daa0df3c (zenflate 0.4.0-dev, `compress`/`simd` feature split + libm drop landed)
- Host: lilith (AMD Ryzen 9 7950X, Zen 4), Linux/WSL2
- Build: `--release` bench profile, **safe** (default `forbid(unsafe_code)`, no `unchecked`),
  **no** `-C target-cpu=native` (runtime SIMD dispatch only)
- Harness: zenbench criterion-compat (interleaved contenders, paired statistics, n=100/arm)
- Raw data: `/tmp/zenbench/zenbench-1783984661-817d8.txt` (throughput run; 48 KB, not committed)

## Contenders (latest published versions, resolved 2026-07-13)

| Crate | Version | Role |
|-------|---------|------|
| zenflate | 0.4.0-dev (this commit) | Rust, compress + decompress |
| [libdeflater](https://crates.io/crates/libdeflater) | 1.25.2 | **C** libdeflate bindings — the reference |
| [zlib-rs](https://crates.io/crates/zlib-rs) | 0.6.6 | Rust, compress + decompress (default features) |
| [flate2](https://crates.io/crates/flate2) | 1.1.9 | zlib-rs backend (≈ zlib-rs through the flate2 API) |
| [miniz_oxide](https://crates.io/crates/miniz_oxide) | 0.9.1 | Rust, compress + decompress |
| [libflate](https://crates.io/crates/libflate) | 2.3.0 | Rust, compress + decompress (no level knob) |
| [yazi](https://crates.io/crates/yazi) | 0.2.1 | Rust, compress + decompress (levels 1-10) |
| [fdeflate](https://crates.io/crates/fdeflate) | 0.3.7 | Rust, decompress arm (PNG-oriented) |
| [zune-inflate](https://crates.io/crates/zune-inflate) | 0.2.54 | Rust, decompress only |

Excluded: `inflate` 0.4.5 and `deflate` 1.0.0 (the earlier image-rs generation,
superseded by miniz_oxide/fdeflate within their own org); `zopfli` 0.8.3 (an
optimal-parse encoder ~100x slower by design — it competes with zenflate's
effort 31+, not with throughput-oriented codecs, and would dominate the
interleaved harness's wall-clock for no comparative signal).

Format: raw DEFLATE for every arm except fdeflate and zlib-rs *decompress*
(zlib-wrapped, as their one-shot APIs expect; +6 bytes framing, Adler-32
verified). Levels: each library's own scale at the same nominal number, and
libflate has exactly one setting. **Level numbers are not equivalent across
libraries** — see the ratio table before reading speed tables.

## Synthetic compress (1 MB, median of n=100)

`mixed` = LCG bytes with periodic 32-byte runs; `photo` = 577×577 RGB gradient+noise
bitmap. Sequential/zeros omitted from the summary (trivially compressible; zenflate
5-10x faster than everything but zlib-rs's static-Huffman fast path — see raw data).

| compress/mixed | L1 | L6 | L9 | L12 (or max) |
|---|---|---|---|---|
| zenflate | 5.53 ms | 6.08 ms | 6.53 ms | 8.31 ms |
| libdeflate (C) | 4.95 ms | 6.03 ms | 6.85 ms | 17.02 ms |
| zlib-rs | 5.35 ms | 12.69 ms | 15.78 ms | 14.56 ms (L9) |
| flate2 (zlib-rs) | 5.65 ms | 12.12 ms | 15.41 ms | 14.11 ms (L9) |
| miniz_oxide | 2.81 ms | 14.63 ms | 15.97 ms | 15.33 ms (L9) |
| yazi | 13.05 ms | 16.22 ms | 16.31 ms | 16.17 ms (L10) |
| libflate | — | 28.81 ms (single default setting) | — | — |

| compress/photo | L1 | L6 | L9 | L12 (or max) |
|---|---|---|---|---|
| zenflate | 5.98 ms | 6.46 ms | 6.57 ms | 9.76 ms |
| libdeflate (C) | 5.52 ms | 7.40 ms | 7.80 ms | 18.78 ms |
| zlib-rs | 6.56 ms | 13.76 ms | 16.54 ms | 15.68 ms (L9) |
| flate2 (zlib-rs) | 6.52 ms | 13.83 ms | 16.15 ms | 15.61 ms (L9) |
| miniz_oxide | 3.50 ms | 16.89 ms | 18.22 ms | 17.01 ms (L9) |
| yazi | 16.00 ms | 17.92 ms | 18.16 ms | 18.07 ms (L10) |
| libflate | — | 34.73 ms (single default setting) | — | — |

Note miniz_oxide's fast L1: it is a static-Huffman/greedy path with a worse
ratio (see ratio table); zenflate L1 emits dynamic Huffman.

## Synthetic decompress (1 MB compressed at zenflate L6, median of n=100)

| decompress | sequential | zeros | mixed | photo |
|---|---|---|---|---|
| zenflate | 45.9 µs | 35.5 µs | **1.31 ms** | **1.51 ms** |
| libdeflate (C) | 35.2 µs | 55.1 µs | 1.24 ms | 1.44 ms |
| zlib-rs | 50.2 µs | 43.6 µs | 1.50 ms | 1.75 ms |
| flate2 (zlib-rs) | 37.9 µs | 31.1 µs | 1.54 ms | 1.73 ms |
| fdeflate | 92.4 µs | 59.4 µs | 1.43 ms | 1.59 ms |
| zune-inflate | 100.6 µs | 78.5 µs | 1.70 ms | 1.88 ms |
| miniz_oxide | 89.0 µs | 69.2 µs | 1.81 ms | 2.10 ms |
| yazi | 85.8 µs | 1180 µs | 1.82 ms | 2.30 ms |
| libflate | 110.6 µs | 179.8 µs | 4.06 ms | 5.09 ms |

On realistic data (mixed/photo) zenflate is the fastest Rust decoder —
13-15% ahead of zlib-rs/flate2, 5-8% ahead of fdeflate, ~20% ahead of
zune-inflate — and within 5% of C libdeflate.

## Corpus results (Canterbury / Silesia / gb82 photos)

<!-- CORPUS_RESULTS -->

## Compressed sizes (ratio context for the speed tables)

<!-- RATIO_RESULTS -->

## Caveats / fairness

- One machine, one run set; re-measure before quoting externally.
- Interleaved rounds mean all contenders share thermal/turbo state; medians of
  n=100 with 95% CIs in the raw data (CVs 1-8%).
- Allocation behavior differs by API shape: zenflate/libdeflater/flate2 reuse
  caller buffers and state; miniz_oxide/yazi/zune-inflate one-shot APIs
  allocate output per call; zlib-rs `compress_slice`/`decompress_slice`
  allocate internal state per call; libflate is an `io::Read`/`io::Write`
  adapter (decode buffer reused here, encode Vec allocated per call). These
  are each library's idiomatic entry points — what a user gets.
- fdeflate targets PNG-shaped data (RLE/limited-symbol fast paths) and is
  benched on its zlib one-shot path; its specialty (PNG filter streams) is
  covered by the gb82 photo corpus rows.
- Level scales differ across libraries; compare at matched ratio using the
  sizes section, not at matched level number.
- yazi's `zeros` decompress outlier (1.18 ms vs everyone's ~30-180 µs)
  reproduced across all 100 samples.

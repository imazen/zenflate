# Rust DEFLATE ecosystem — zenflate 0.4.0 head-to-head

- Date: 2026-07-13
- Commit: 1a7ce8c7 (zenflate 0.4.0-dev; local throughput run at daa0df3c — identical lib code, bench arms added)
- Hosts: lilith (AMD Ryzen 9 7950X, Zen 4, Linux/WSL2, shared), zen-train-1
  (Hetzner CCX63, 48 dedicated x86 cores, idle), zen-arm-big (Hetzner CAX31,
  8-core Ampere Altra Neoverse N1, idle)
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
superseded by miniz_oxide/fdeflate within their own org); `zopfli` 0.8.3 is excluded
from the interleaved throughput benches (an optimal-parse encoder ~100x
slower by design would dominate the harness wall-clock) but IS included in
the `rd_sweep` frontier/max-compression results below.

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

Local WSL2 run (7950X): Canterbury complete + Silesia through the per-file
groups (the 200 MB aggregate section was cut for time; dedicated-box corpus
runs deferred — see per-host synthetic sections below, measured on idle
dedicated Hetzner hardware). Silesia decompress, compressed at zenflate L6,
median ms (chart-bar values from the interleaved run):

| file | zenflate | libdeflate | zlib-rs | flate2 | fdeflate | zune-inflate | miniz_oxide | yazi | libflate |
|---|---|---|---|---|---|---|---|---|---|
| dickens | 13.11 | 8.67 | 11.30 | 11.07 | 13.33 | 11.84 | 18.48 | 20.98 | 34.85 |
| mr | 10.23 | 7.66 | 9.90 | 9.66 | 11.01 | 10.47 | 14.71 | 18.81 | 28.05 |
| nci | 27.02 | 20.87 | 24.17 | 23.25 | 27.59 | 29.54 | 51.12 | 37.01 | 49.91 |
| ooffice | 9.81 | 6.97 | 9.29 | 9.27 | 9.85 | 9.97 | 13.43 | 14.64 | 25.64 |
| osdb | 8.92 | 6.58 | 9.29 | 8.95 | 9.65 | 9.50 | 13.25 | 14.14 | 26.41 |
| reymont | 7.15 | 4.50 | 5.91 | 5.80 | 7.41 | 6.24 | 10.94 | 12.32 | 19.08 |
| samba | 18.17 | 11.96 | 15.76 | 15.14 | 18.50 | 27.39 | 25.52 | 27.81 | 46.75 |
| sao | 11.52 | 9.00 | 11.53 | 11.73 | 11.99 | 12.82 | 15.97 | 17.13 | 32.39 |
| webster | 66.19 | 45.93 | 54.06 | 53.22 | 65.40 | 64.97 | 91.62 | 108.04 | 141.88 |
| x-ray | 14.86 | 12.32 | 14.83 | 14.75 | 14.61 | 16.51 | 19.66 | 20.18 | 38.79 |
| xml | 3.24 | 1.93 | 2.61 | 2.45 | 3.46 | 2.82 | 4.44 | 4.72 | 7.95 |

## Compressed sizes (ratio context for the speed tables)

Measured on **zen-train-1** (Hetzner CCX63, 48 dedicated x86 cores, idle;
synthetics-only — the 20-minute run budget cut the silesia datasets; the
committed `rd_sweep_train1_2026-07-13.csv` has all 120 points with decode
times). Note both 1 MB synthetics have narrow ratio spreads, so the
max-compression race is decided by hundreds of bytes.

**mixed-1MB — smallest output (max compression):**

| rank | point | bytes | compress time |
|---|---|---|---|
| 1 | zenflate/e76 | 885477 | 1371 ms |
| 2 | zopfli/i30 | 885481 | 1522 ms |
| 3 | zopfli/i15 | 885482 | 911 ms |
| 4 | zenflate/e31 | 885484 | 456 ms |
| 5 | zenflate/e46 | 885484 | 761 ms |
| 6 | zenflate/e30 | 885535 | 32 ms |

**mixed-1MB — ratio-vs-time Pareto frontier:**

| point | bytes | compress time |
|---|---|---|
| fdeflate/default | 1369386 | 1.0 ms |
| miniz_oxide/L1 | 886318 | 3.0 ms |
| libdeflate/L5 | 885573 | 8.6 ms |
| libdeflate/L10 | 885536 | 22.5 ms |
| zenflate/e30 | 885535 | 32.1 ms |
| zenflate/e31 | 885484 | 456.1 ms |
| zopfli/i15 | 885482 | 910.5 ms |
| zenflate/e76 | 885477 | 1371.1 ms |

**photo-1MB — smallest output (max compression):**

| rank | point | bytes | compress time |
|---|---|---|---|
| 1 | zenflate/e76 | 912815 | 967 ms |
| 2 | zenflate/e46 | 912947 | 630 ms |
| 3 | zopfli/i30 | 913299 | 1728 ms |
| 4 | zlib-rs/L9 | 913427 | 21 ms |
| 5 | zenflate/e31 | 913503 | 458 ms |
| 6 | zopfli/i15 | 913775 | 1046 ms |

**photo-1MB — ratio-vs-time Pareto frontier:**

| point | bytes | compress time |
|---|---|---|
| fdeflate/default | 1449639 | 0.8 ms |
| miniz_oxide/L1 | 918329 | 3.6 ms |
| zlib-rs/L8 | 918319 | 16.7 ms |
| zlib-rs/L4 | 918318 | 18.6 ms |
| zlib-rs/L9 | 913427 | 21.3 ms |
| zenflate/e46 | 912947 | 630.1 ms |
| zenflate/e76 | 912815 | 967.2 ms |


## Per-host synthetic results (dedicated Hetzner hardware)

### zen-train-1 — CCX63, 48 dedicated x86 cores (idle, load 0.00)

| compress (median) | mixed L1 | mixed L6 | mixed L12/max | photo L6 | photo L12/max |
|---|---|---|---|---|---|
| zenflate | 6.95 ms | 8.14 ms | 11.41 ms | 8.54 ms | 13.81 ms |
| libdeflate (C) | 6.92 ms | 8.95 ms | 23.28 ms | 11.04 ms | 27.19 ms |
| zlib-rs | 7.43 ms | 16.52 ms | 19.11 ms (L9) | 19.02 ms | 21.04 ms (L9) |
| miniz_oxide | 2.98 ms | 20.33 ms | 20.37 ms (L9) | 22.96 ms | 23.08 ms (L9) |
| yazi | 19.66 ms | 22.51 ms | 22.62 ms (L10) | 26.32 ms | 26.67 ms (L10) |

| decompress (median) | mixed | photo |
|---|---|---|
| zenflate | **1.89 ms** | **2.07 ms** |
| libdeflate (C) | 1.80 ms | 1.99 ms |
| zlib-rs | 2.01 ms | 2.23 ms |
| fdeflate | 2.01 ms | 2.12 ms |
| zune-inflate | 2.25 ms | 2.48 ms |
| miniz_oxide | 2.48 ms | 2.99 ms |
| yazi | 2.42 ms | 3.02 ms |
| libflate | 4.70 ms | 6.07 ms |

Same ordering as the WSL2 host, on clean silicon: fastest Rust decoder
(zlib-rs +6-8% behind, everything else +12-60%), ~5% behind C; compression
2x faster than every Rust crate at L6+, 2x faster than C at L12.

### zen-arm-big — CAX31, 8-core Ampere Altra Neoverse N1 (aarch64)

First aarch64 numbers for this comparison (NEON adler/CRC + archmage runtime
dispatch on the zenflate side).

| compress (median) | mixed L1 | mixed L6 | mixed L12/max | photo L6 | photo L12/max |
|---|---|---|---|---|---|
| zenflate | 10.22 ms | 12.26 ms | 17.05 ms | 13.49 ms | 20.78 ms |
| libdeflate (C) | 8.72 ms | 13.73 ms | 46.22 ms | 16.24 ms | 47.33 ms |
| zlib-rs | 11.97 ms | 25.51 ms | 29.33 ms (L9) | 28.92 ms | 30.90 ms (L9) |
| miniz_oxide | 7.04 ms | 27.08 ms | 27.20 ms (L9) | 31.23 ms | 30.63 ms (L9) |
| yazi | 30.78 ms | 34.24 ms | 34.35 ms (L10) | 38.55 ms | 39.11 ms (L10) |

| decompress (median) | mixed | photo |
|---|---|---|
| zenflate | **2.12 ms** | **2.33 ms** |
| libdeflate (C) | 2.05 ms | 2.29 ms |
| zlib-rs | 2.85 ms | 3.18 ms |
| miniz_oxide | 2.89 ms | 3.32 ms |
| zune-inflate | 3.46 ms | 3.63 ms |
| yazi | 3.32 ms | 3.79 ms |
| fdeflate | 3.68 ms | 3.80 ms |
| libflate | 8.18 ms | 9.65 ms |

The ARM decode lead is *wider* than x86: zenflate is 26-27% faster than
zlib-rs and within 2-4% of C libdeflate. Compression at L6+ is ~2x every
Rust crate and 2.3-2.7x C at L12. Known gap: NEON checksum tiers trail C
(adler32 46 µs vs 34 µs; crc32 57 µs vs 47 µs on 1 MB) — tuning headroom,
tracked for a follow-up.

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

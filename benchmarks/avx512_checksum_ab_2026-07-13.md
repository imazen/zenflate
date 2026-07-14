# AVX-512 checksum tier: is the `avx512` feature worth defaulting on?

**Date:** 2026-07-13
**Host:** AMD Ryzen 9 7950X (Zen 4 — native AVX-512F/BW/DQ/VL + VNNI + VPCLMULQDQ), WSL2
**Build:** release, `lto="thin"`, **no** `-C target-cpu=native` (runtime dispatch, as shipped)
**Commit:** be93a31d (pre-change measurement)
**Harnesses:** `scratchpad/cksum-ab` (checksum-only), `scratchpad/pipe-ab` (gzip pipeline)

## What `avx512` gates

Exactly two code paths, both checksum-only:

- `adler32_impl_v4x` — AVX-512 VNNI 512-bit (`vpdpbusd zmm`)
- `crc32_impl_v4x` — VPCLMULQDQ 512-bit zmm folding

Dispatch tiers (`incant!`):
- Adler-32: **with** `avx512` → `[v4x, v4, v3, …]`; **without** → `[v3, …]` (AVX2 max)
- CRC-32:   **with** `avx512` → `[v4x, x64_crypto, …]`; **without** → `[x64_crypto, …]` (PCLMULQDQ 128-bit max)

Nothing in compress or decompress core loops uses `avx512`.

## Standalone checksum throughput (GiB/s, median of 5 interleaved rounds)

| size | adler avx2 | adler v4x | a× | crc avx2 | crc v4x | c× |
|-----:|-----------:|----------:|---:|---------:|--------:|---:|
| 64K  | 84.5 | 136.5 | 1.62× | 18.4 | 79.0 | **4.29×** |
| 256K | 83.0 | 134.5 | 1.62× | 18.5 | 80.1 | **4.33×** |
| 1M   | 77.4 | 112.8 | 1.46× | 18.4 | 78.2 | **4.25×** |
| 4M   | 80.5 | 120.7 | 1.50× | 18.4 | 78.5 | **4.27×** |
| 16M  | 76.8 |  83.4 | 1.09× | 18.3 | 75.5 | **4.13×** |

- **CRC-32: consistent ~4.3×** (128-bit PCLMULQDQ → 512-bit VPCLMULQDQ). CRC folding stays compute-bound even at 16 MiB.
- **Adler-32: 1.1–1.6×** — big when cache-resident, converges to ~1.1× once memory-bound at 16 MiB.

## gzip pipeline throughput (GiB/s on uncompressed bytes, median of 5 rounds, effort 15)

| file | comp avx2 | comp v4x | c× | dec avx2 | dec v4x | d× |
|-----:|----------:|---------:|---:|---------:|--------:|---:|
| xml (5.3M)   | 0.147 | 0.153 | 1.04× | 1.819 | 2.006 | **1.10×** |
| nci (33M)    | 0.204 | 0.203 | 1.00× | 1.128 | 1.144 | 1.01× |
| x-ray (8.5M) | 0.105 | 0.104 | 0.99× | 0.474 | 0.481 | 1.01× |

- **Compression: 0%** — CRC is negligible next to the encoder (encode is ~100× slower than even the 18 GiB/s CRC).
- **Decompression: 1–10%** — the 10% is xml only (fastest-decoding, most-compressible → CRC is the largest slice of decode time). Typical data ~1%.

## Cost of keeping `avx512` on (measured, not assumed)

| axis | with avx512 | without | delta |
|------|-------------|---------|-------|
| dep tree (crates, no-dev) | 13 | 13 | **+0 crates** |
| cold build (median of 4, fresh target) | 3.21s | 3.19s | **+0.02s (+1%)** |
| release binary (this harness) | 495,464 B | 488,296 B | **+7 KB** |
| default-build MSRV | 1.89 | 1.89 | **+0** — archmage (pulled by `simd`) already requires 1.89 |

`archmage/avx512` toggles codegen **inside archmage** (already a dependency); it adds no crates. The three
`#[allow(clippy::incompatible_msrv)]` sites are all in the avx512 paths, but they don't set the crate MSRV
because `simd` → archmage 1.89 does.

## Verdict

Making `avx512` opt-in removes **no** dependency, saves **~1%** build time, saves **7 KB**, and does **not**
lower the MSRV — while costing **4.3× standalone CRC-32**, 1.5× standalone Adler-32, and up to 10% gzip
decompress on fast/compressible data. For the DEFLATE pipeline the effect is 0–10% (typically ~1%); the 4×
only matters if zenflate is used as a standalone checksum library.

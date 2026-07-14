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

## Appendix: raw per-round data

Harness sources: `benchmarks/harnesses/cksum-ab.rs` and `benchmarks/harnesses/pipe-ab.rs`
(each depends on `zenflate` by path, built twice with `--features avx2` / `--features avx512`).
Each round is one full pass of both tiers, run interleaved (`nice -n19`) to cancel thermal drift.

### Checksum A/B (GiB/s per size, 5 rounds)

```
########## round 1
# tier=avx2(v3)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,84.7,18.4
256,83.3,18.5
1024,77.3,18.3
4096,80.0,18.4
16384,77.2,18.3
# tier=avx512(v4x)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,136.5,78.6
256,134.5,79.0
1024,109.2,75.1
4096,120.7,78.1
16384,78.8,69.4
########## round 2
# tier=avx2(v3)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,84.4,18.3
256,83.0,18.5
1024,77.5,18.4
4096,80.5,18.2
16384,75.8,18.3
# tier=avx512(v4x)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,137.0,79.6
256,135.6,80.2
1024,113.1,78.2
4096,120.5,78.5
16384,86.8,75.5
########## round 3
# tier=avx2(v3)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,84.5,18.4
256,83.0,18.5
1024,77.1,18.3
4096,81.0,18.3
16384,76.8,18.3
# tier=avx512(v4x)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,135.5,77.3
256,134.8,80.9
1024,112.8,78.6
4096,120.9,78.6
16384,82.9,75.9
########## round 4
# tier=avx2(v3)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,84.6,18.4
256,82.9,18.5
1024,77.6,18.4
4096,80.4,18.4
16384,77.1,18.2
# tier=avx512(v4x)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,138.4,79.6
256,128.6,80.1
1024,113.2,79.3
4096,121.4,79.2
16384,83.4,76.1
########## round 5
# tier=avx2(v3)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,84.4,18.5
256,82.9,18.5
1024,77.4,18.4
4096,81.7,18.4
16384,75.3,18.3
# tier=avx512(v4x)  per_sample=512MiB  samples=15
size_kib,adler32_gibs,crc32_gibs
64,136.3,79.0
256,134.5,79.6
1024,111.7,77.8
4096,119.6,78.5
16384,84.7,75.1
```

### gzip pipeline A/B (GiB/s on uncompressed bytes, 5 rounds)

```
########## round 1
# tier=avx2  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.147,1.819
nci,33.6,0.211,1.148
x-ray,8.5,0.105,0.478
# tier=avx512  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.153,2.012
nci,33.6,0.197,1.105
x-ray,8.5,0.104,0.481
########## round 2
# tier=avx2  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.148,1.823
nci,33.6,0.197,1.110
x-ray,8.5,0.103,0.474
# tier=avx512  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.154,2.006
nci,33.6,0.206,1.190
x-ray,8.5,0.101,0.474
########## round 3
# tier=avx2  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.152,1.774
nci,33.6,0.201,1.113
x-ray,8.5,0.105,0.451
# tier=avx512  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.146,1.924
nci,33.6,0.196,1.111
x-ray,8.5,0.102,0.464
########## round 4
# tier=avx2  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.147,1.752
nci,33.6,0.204,1.173
x-ray,8.5,0.109,0.497
# tier=avx512  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.153,2.012
nci,33.6,0.205,1.162
x-ray,8.5,0.106,0.489
########## round 5
# tier=avx2  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.144,1.820
nci,33.6,0.204,1.128
x-ray,8.5,0.104,0.474
# tier=avx512  gzip effort=15 (balanced)  median-of-9
file,mib,comp_gibs,decomp_gibs
xml,5.3,0.151,1.997
nci,33.6,0.203,1.144
x-ray,8.5,0.104,0.488
```

# zenflate

Pure Rust DEFLATE/zlib/gzip compression and decompression.

## Architecture
- Built on libdeflate's core algorithms, extended with Zopfli-style optimal parsing, multi-strategy Huffman optimization, and original matchfinder designs
- Safe Rust (forbid(unsafe_code) by default)
- Opt-in `unchecked` feature flag for bounds-check elimination in hot paths
- SIMD via archmage/magetypes
- Side-by-side testing against C via `libdeflater` crate

## Source Reference
- C source: `/home/lilith/work/libdeflate-src/lib/`

## Module Map
- `src/constants.rs` — DEFLATE format constants (from deflate_constants.h)
- `src/error.rs` — Error types
- `src/checksum/` — Adler-32 and CRC-32 (scalar + SIMD)
- `src/decompress/` — Decompression (bitstream reader, Huffman tables, inflate loop, streaming)
- `src/compress/` — Compression (bitstream writer, Huffman construction, block flushing, strategies)
- `src/fast_bytes.rs` — Unchecked byte load/store helpers (cfg-gated)
- `src/matchfinder/` — Hash table, hash chain, binary tree matchfinders
- `src/decompress/mod.rs` — gzip/zlib wrappers integrated into Decompressor

## Implementation Status
- [x] Phase 1: Foundation + Checksums (Adler-32, CRC-32 scalar, 23 parity tests)
- [x] Phase 2: Decompression (generic loop, all 3 formats, 10 parity tests at all levels)
- [x] Phase 3: Compression Core (bitstream writer, Huffman construction, block flushing, 55 tests)
- [x] Phase 4: Compression Strategies (levels 0-12: fastest, greedy, lazy, lazy2, near-optimal; 97 tests)
- [x] Phase 5: SIMD Acceleration
  - Adler-32: AVX-512 VNNI 512-bit (v4x) + AVX-512 (v4) + AVX2 (v3) + NEON + WASM simd128 — 123 GiB/s (1.01x C)
  - CRC-32: PCLMULQDQ 128-bit (v2) + VPCLMULQDQ 512-bit zmm (modern) + PMULL (aarch64, NeonAes) — 78 GiB/s (1.00x C)
  - Decompression fastloop + optimized match copy
- [x] Phase 6: Benchmarks + Polish (criterion benchmarks, README, doc examples, #[non_exhaustive] errors)
- [x] Phase 7: Ecosystem benchmarks (flate2, miniz_oxide), justfile, Dockerfile, CI bench checks
- [x] Phase 8: Streaming decompression (StreamDecompressor, InputSource trait, fill/peek/advance API, BufRead/Read impls, 15 tests)
- [x] Phase 9: Effort-based compression (0-30) with new strategies
  - CompressionLevel::new(effort) with effort 0-30, Pareto-ranked
  - CompressionLevel::libdeflate(level) for byte-identical C parity (0-12)
  - Turbo (effort 1-4): dynamic Huffman + single-entry hash, limited skip updates
  - FastHt (effort 5-7): dynamic Huffman + 2-entry hash, limited skip updates
  - Named presets: none(), fastest(), fast(), balanced(), high(), best()
  - 195 tests + 10 doctests pass


## Standard Corpus Compression (--features unchecked)

Canterbury (11 text files), Silesia (11 mixed files), gb82 photos (5 raw RGB).
Aggregate throughput across all files in each corpus.

| Corpus | Level | zenflate | C | flate2 | vs C | vs flate2 |
|--------|-------|----------|---|--------|------|-----------|
| Canterbury | L1 | 431 MiB/s | 381 MiB/s | 441 MiB/s | **1.13x** | 0.98x |
| Canterbury | L6 | 347 MiB/s | 131 MiB/s | 108 MiB/s | **2.66x** | **3.21x** |
| Canterbury | L12 | 147 MiB/s | 8 MiB/s | 15 MiB/s | **19.6x** | **9.69x** |
| Silesia | L1 | 365 MiB/s | 319 MiB/s | 376 MiB/s | **1.14x** | 0.97x |
| Silesia | L6 | 289 MiB/s | 128 MiB/s | 96 MiB/s | **2.25x** | **3.00x** |
| Silesia | L12 | 146 MiB/s | 7 MiB/s | 47 MiB/s | **19.7x** | **3.09x** |
| Photos (RGB) | L1 | 222 MiB/s | 193 MiB/s | 178 MiB/s | **1.15x** | **1.24x** |
| Photos (RGB) | L6 | 162 MiB/s | 114 MiB/s | 70 MiB/s | **1.42x** | **2.31x** |
| Photos (RGB) | L12 | 99 MiB/s | 18 MiB/s | 44 MiB/s | **5.53x** | **2.23x** |

Note: flate2 in benchmarks uses zlib-rs backend. At L1, flate2 uses static
Huffman + 4K hash table (faster but worse ratio than zenflate's dynamic Huffman).
zenflate L6 is 2-3x faster than both C and flate2 across all corpus types.
L12 gap vs C is huge because zenflate's near-optimal is a different algorithm.

### Per-file Silesia L6 (--features unchecked)

| File | zenflate | C | flate2 | vs C | vs flate2 |
|------|----------|---|--------|------|-----------|
| dickens (10M text) | 211 MiB/s | 77 MiB/s | 65 MiB/s | **2.73x** | **3.25x** |
| nci (33M chemistry) | 832 MiB/s | 277 MiB/s | 224 MiB/s | **3.01x** | **3.71x** |
| reymont (6.6M text) | 285 MiB/s | 85 MiB/s | 54 MiB/s | **3.34x** | **5.30x** |
| samba (21M source) | 381 MiB/s | 149 MiB/s | 112 MiB/s | **2.55x** | **3.40x** |
| sao (7M binary) | 146 MiB/s | 82 MiB/s | 60 MiB/s | **1.78x** | **2.45x** |
| webster (41M dict) | 272 MiB/s | 109 MiB/s | 82 MiB/s | **2.51x** | **3.32x** |
| x-ray (8.5M image) | 143 MiB/s | 127 MiB/s | 72 MiB/s | **1.12x** | **1.99x** |
| xml (5.3M data) | 567 MiB/s | 211 MiB/s | 170 MiB/s | **2.69x** | **3.33x** |

### Photo bitmap all levels (3 MiB, --features unchecked)

| Level | Unchecked | C | vs C |
|-------|-----------|---|------|
| L1 | 216 MiB/s | 185 MiB/s | **1.17x** |
| L2 | 214 MiB/s | 130 MiB/s | **1.65x** |
| L4 | 215 MiB/s | 130 MiB/s | **1.65x** |
| L6 | 149 MiB/s | 128 MiB/s | **1.17x** |
| L9 | 152 MiB/s | 122 MiB/s | **1.25x** |
| L10 | 102 MiB/s | 56 MiB/s | **1.81x** |
| L12 | 98 MiB/s | 48 MiB/s | **2.06x** |

Byte-identical output at every level.

### Ecosystem comparison (3 MiB photo bitmap, unchecked)

| Library | Level | Speed |
|---------|-------|-------|
| zenflate | 6 | 149 MiB/s |
| zenflate | 9 | 152 MiB/s |
| zenflate | 12 | 98 MiB/s |
| flate2 (zlib-rs) | 1 | 144 MiB/s |
| flate2 (zlib-rs) | best | 56 MiB/s |
| miniz_oxide | best | 56 MiB/s |

zenflate L6 is ~2.7x faster than flate2/miniz_oxide at comparable ratios.

### Synthetic data compression (1MB, --features unchecked)

| Level | Data | zenflate | libdeflate C | Ratio |
|-------|------|----------|-------------|-------|
| L1 | mixed | 4ms | 5ms | **1.15x** |
| L6 | mixed | 6ms | 6ms | **1.02x** |
| L12 | mixed | 8ms | 18ms | **2.25x** |
| L1 | photo | 4ms | 5ms | **1.17x** |
| L6 | photo | 6ms | 7ms | **1.17x** |
| L12 | photo | 10ms | 20ms | **2.06x** |

Sequential/zeros data omitted — too synthetic (zenflate 5-14x faster than C
due to Turbo matchfinder's limited-skip strategy on ultra-repetitive patterns).

### Parallel Compression (4MB mixed data, --features unchecked)

| Level | 1 thread | 2 threads | 4 threads | Speedup (4T) |
|-------|----------|-----------|-----------|--------------|
| L1 | 16.6ms | 9.3ms | 5.3ms | **3.1x** |
| L6 | 23.9ms | 13.1ms | 7.2ms | **3.3x** |
| L12 | 32.3ms | 17.3ms | 9.5ms | **3.4x** |

Parallel compression uses pigz-style chunking: equal-sized chunks with 32KB
dictionary overlap, sync flush at boundaries, combined CRC-32 via GF(2) matrix.

### Decompression (1MB, compressed at L6, unchecked)

| Data type | zenflate | libdeflate (C) | fdeflate | zlib-rs | flate2 (zlib-rs) | miniz_oxide |
|-----------|----------|----------------|----------|---------|--------|-------------|
| Sequential | 44µs (0.80x) | 36µs | 90µs | 49µs | 36µs | 108µs |
| Zeros | 34µs (**1.60x**) | 55µs | 55µs | 42µs | 29µs | 79µs |
| Mixed | 1.3ms (0.93x) | 1.2ms | 1.4ms | 1.5ms | 1.5ms | 1.7ms |
| Photo | 1.5ms (0.94x) | 1.4ms | 1.5ms | 1.7ms | 1.7ms | 2.0ms |

Note: `unchecked` feature does NOT help decompression — safe bounds checks
give LLVM information that enables better optimization (+5-6% regression when
using `get_unchecked` for table lookups and match copy).
flate2 now uses zlib-rs backend, which is much faster than old miniz_oxide backend.

### Corpus Decompression (L6, unchecked, selected files)

| File | zenflate | C | fdeflate | zlib-rs | flate2 | vs C |
|------|----------|---|----------|---------|--------|------|
| dickens (10M) | 712 MiB/s | 1048 MiB/s | 736 MiB/s | 776 MiB/s | 786 MiB/s | 0.68x |
| samba (21M) | 1105 MiB/s | 1592 MiB/s | 1071 MiB/s | 1231 MiB/s | 1276 MiB/s | 0.69x |
| xml (5.3M) | 1636 MiB/s | 2603 MiB/s | 1497 MiB/s | 1924 MiB/s | 1957 MiB/s | 0.63x |
| sao (7M binary) | 625 MiB/s | 764 MiB/s | 546 MiB/s | 598 MiB/s | 615 MiB/s | 0.82x |
| x-ray (8.5M) | 566 MiB/s | 667 MiB/s | 536 MiB/s | 517 MiB/s | 553 MiB/s | 0.85x |
| dog (photo RGB) | 595 MiB/s | 633 MiB/s | 570 MiB/s | 539 MiB/s | 553 MiB/s | 0.94x |

Decompression gap vs C: 0.63-0.94x across real-world data. Largest gap on
highly compressible text (xml, dickens); smallest on binary/photo data.

### Streaming Decompression (1MB, compressed at L6, safe mode)

| Data type | zenflate whole | zenflate stream (64K) | zenflate stream (4K) | fdeflate |
|-----------|----------------|----------------------|---------------------|----------|
| Sequential | 45µs | 56µs (1.25x whole) | 109µs | 89µs |
| Zeros | 34µs | 53µs (1.53x whole) | 103µs | 54µs |
| Mixed | 1ms | 2ms (1.16x whole) | 2ms | 1ms |
| Photo | 1ms | 2ms (1.14x whole) | 2ms | 2ms |

Streaming overhead vs whole-buffer: 14-53% with 64K capacity.

### Checksums (1MB sequential, --features unchecked)

| Algorithm | zenflate | libdeflate (C) | Ratio |
|-----------|----------|----------------|-------|
| Adler-32 | 123 GiB/s | 121 GiB/s | **1.01x** |
| CRC-32 | 78 GiB/s | 78 GiB/s | **1.00x** |

## Investigation Notes

### L1 +48% instruction overhead (callgrind)
- NOT panic-related (zero panic calls in assembly)
- Root cause: register pressure from fat pointers + separate hash table allocation
- 18 stack spills in Rust hot loop vs 2 in C
- Stack frame: 232 bytes Rust vs 104 bytes C (2.2x)
- Raw pointers DON'T help (LLVM already proves bounds for simple 2-entry hash table)
- Embedding HtMatchfinder inline in Compressor struct does NOT help — regressed +14.8%
  - 805 asm lines (vs 744), 124 stack refs (vs 112), 248 byte frame (vs 232)
  - Hash table at offset 0x11f8 from self means larger displacements everywhere
  - LLVM may lose noalias reasoning (Box = separate object, inline = same object)
- `ht.rs` has `longest_match_raw`/`skip_bytes_raw` available but unused

### Callgrind instruction counts (all levels, unchecked, 1MB sequential)
| Level | zenflate | C | Overhead |
|-------|---------|---|----------|
| L1 | 86.8M | 58.5M | +48% |
| L6 | 116.0M | 93.1M | +25% |
| L9 | 117.9M | 96.3M | +22% |
| L12 | 361.4M | 302.3M | +20% |

Cachegrind: D1 cache misses nearly identical. Gap is pure instruction count.

### Decompression: unchecked hurts, SIMD won't help
- `get_unchecked` in table lookups, literal stores, match copy REGRESSES 5-6% on mixed/photo
- LLVM uses safe bounds checks to prove variable relationships → better codegen
- Assembly confirms: unchecked has fewer lines (2020 vs 2146) and panic sites (18 vs 24), yet slower
- C libdeflate does NOT use explicit SIMD for decompression match copy either
- Only x86-specific decompression opt in C is BMI2 BZHI for bit extraction
- Decompression gap vs C is from register pressure / instruction count, not SIMD

## Known Bugs

(none currently)

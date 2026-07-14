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
- [x] Phase 9: Effort-based compression (0-200) with new strategies
  - CompressionLevel::new(effort) with effort 0-200; 0-30 Pareto-ranked,
    31-200 = Zopfli-style FullOptimal (iterations = effort − 16)
  - CompressionLevel::libdeflate(level) for byte-identical C parity (0-12)
  - Turbo (effort 1-4): dynamic Huffman + single-entry hash, limited skip updates
  - FastHt (effort 5-7): dynamic Huffman + 2-entry hash, limited skip updates
  - Named presets: none(), fastest(), fast(), balanced(), high(), best()
  - 195 tests + 10 doctests pass
- [x] Phase 10 (0.4.0): Feature split — `compress` (compression + matchfinders,
  implies alloc), `simd` (archmage optional; scalar checksums without it),
  libm dropped (std → f64::log2, no_std → local log2_series). Decode-only
  build (`--no-default-features --features std`): 1 dep (enough), cold build
  0.28s debug / 0.30s release vs 3.2s/3.4s full-default, 105 lib tests.
  BREAKING for default-features=false users: add `compress`/`simd` as needed.


## Performance (0.4.0)

**Source of truth = the committed dated runs in `benchmarks/`.** Do not relabel
them as newer than their date; re-run to refresh. The inline numbers below are
headlines measured on **0.4.0, AMD Ryzen 9 7950X (Zen 4), WSL2, safe (default),
no `-C target-cpu=native`** — the full tables live in:

- `benchmarks/deflate_rust_ecosystem_2026-07-13.md` — 0.4.0 vs the whole Rust
  ecosystem (libdeflater C / zlib-rs / flate2 / miniz_oxide / fdeflate /
  zune-inflate / libflate / yazi), **3 hosts** (7950X WSL2, Hetzner CCX63 x86,
  Hetzner CAX31 aarch64): synthetic compress/decompress, Silesia per-file
  decompress, max-compression race, dickens real-text reality check.
- `benchmarks/avx512_checksum_ab_2026-07-13.md` — checksum SIMD tiers (v4x vs v3),
  gzip pipeline impact, build/dep cost (why `avx512` stays default-on).
- `benchmarks/rd_sweep_train1_2026-07-13.csv` — 120 roundtrip-verified RD points.
- `benchmarks/image_deflate_corpus_2026-06-18.txt`,
  `benchmarks/zenflate_vs_zlibrs_2026-06-13.md`, `benchmarks/README.md` — older
  PNG-residual + matched-ratio runs + methodology.

### Compression (lower = faster)

1 MB mixed synthetic, median of n=100 (interleaved zenbench):

| | L1 | L6 | L12 / max |
|---|---|---|---|
| zenflate | 5.53 ms | 6.08 ms | 8.31 ms |
| libdeflate (C) | 4.95 ms | 6.03 ms | 17.02 ms |
| zlib-rs | 5.35 ms | 12.69 ms | 14.56 ms (L9) |
| miniz_oxide | 2.81 ms | 14.63 ms | 15.33 ms (L9) |

≈C at L6, ~2× every Rust crate at L6+, ~2× C at L12 (different near-optimal
algorithm). Byte-identical to C at every level via `CompressionLevel::libdeflate(n)`.
3 MB near-incompressible synthetic photo (`examples/ratio_bench.rs`, safe): zenflate
e1 172 / e15 88 / e30 10 MiB/s vs libdeflate L1 178 / L9 114 / L12 43 — this is the
worst case for effort near-optimal (real corpora invert it; see ecosystem file).
`unchecked` adds +0–12% at L1, +0–6% at L6+ (compression only).

### Decompression (higher = faster; `unchecked` does NOT help — safe is equal/faster)

1 MB compressed at zenflate L6:

| Data | zenflate | libdeflate (C) | flate2 (zlib-rs) | miniz_oxide |
|---|---|---|---|---|
| Sequential | 21.3 GiB/s | 27.7 GiB/s | 25.8 GiB/s | 11.0 GiB/s |
| Mixed | 763 MiB/s | 806 MiB/s | 649 MiB/s | 552 MiB/s |
| Photo | 662 MiB/s | 694 MiB/s | 578 MiB/s | 476 MiB/s |

Fastest Rust decoder on realistic data (13–15% ahead of zlib-rs/flate2, ~20% ahead
of zune-inflate), within ~5% of C. aarch64 lead is *wider* (see ecosystem file).
Streaming decode overhead ≈1.1–1.6× whole-buffer depending on chunk size.

### Checksums (1 MiB sequential, `avx512` default-on)

| Algorithm | zenflate | libdeflate (C) | vs C |
|---|---|---|---|
| Adler-32 | 110 GiB/s | 118 GiB/s | 0.93× |
| CRC-32 | 77 GiB/s | 75 GiB/s | 1.02× |

Standalone 512-bit (v4x) vs 256-bit (v3): CRC-32 4.3×, Adler-32 1.1–1.6× — full
A/B in the avx512 file. Impl: AVX-512 VNNI / PCLMULQDQ (x86), NEON / PMULL
(aarch64), simd128 (WASM).

### Parallel gzip (4 MiB mixed, `gzip_compress_parallel`)

| effort | 1 thread | 4 threads | speedup |
|---|---|---|---|
| e1 | 21 ms | 6.2 ms | 3.4× |
| e15 | 38 ms | 11 ms | 3.5× |
| e30 | 178 ms | 52 ms | 3.4× |

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

### WASM simd128 audit (2026-04-01)
Verified all hot paths auto-vectorize correctly on wasm32 with simd128:

**Already working well:**
- `matchfinder_rebase`: `#[autoversion]` produces `i16x8.add_sat_s` with 4x unrolled loop (64 bytes/iter)
- `matchfinder_init`: `#[autoversion]` produces `v128.store` with 4x unrolled loop
- Adler-32: Explicit `#[arcane]` wasm128 path using `i16x8_extend`/`i32x4_dot_i16x8`/`i32x4_extadd_pairwise`
- `DeflateFreqs::reset`: Compiles to `memory.fill` (WASM bulk memory)
- All slide_window/init functions produce zero-warning, zero-fallback SIMD code

**Added in this audit:**
- `lz_extend`: wasm128 path using `i8x16_ne` + `i8x16_bitmask` for 16-byte match comparison
  (up from 8-byte u64 XOR). Benefits longer matches in image data.

**Not SIMD-amenable (confirmed):**
- CRC-32: No carryless multiply on WASM. Falls back to slice-by-8 (8KB table). No faster
  approach exists without `clmul`-equivalent hardware.
- Hash computation (`lz_hash`): Single-cycle scalar multiply, inherently serial
- Huffman coding/bitstream writing: Bit-serial, data-dependent
- DP backward pass (`find_min_cost_path`): Serial dependency chain
- Frequency counting: Scatter-add pattern, not vectorizable

**Build verification:**
- `RUSTFLAGS="-C target-feature=+simd128" cargo check --target wasm32-unknown-unknown --no-default-features --features alloc` — zero warnings
- All CRC-32 fold constants/macros properly cfg-gated to x86_64/aarch64

### Decode-only / optional-archmage / no-libm feasibility (2026-07-13, scratchpad prototype)

Measured cold builds (7950X, fresh scratch CARGO_TARGET_DIR each run), full crate at default
features vs a prototype containing only decompress + scalar checksums + error + enough:

- Cold build wall time: debug 3.24s → 0.26s (12.5x), release-without-debuginfo 3.43s → 0.32s (10.7x).
  The proc-macro chain (proc-macro2 → quote → syn → archmage-macros → archmage) IS the critical
  path: ~3.0s of the 3.2s debug wall. zenflate itself: 0.82s debug. libm: 0.62s + 0.10s build script.
- Dependency count: 9 crates → 1 (enough). rlib (release, no debuginfo): 2.21 MB → 514 KB.
- Decode-only *binary* size delta is only ~11-14 KB (release-stripped and opt-z/LTO/panic-abort
  probes agree): unused compress code is already linker-DCE'd; the delta is the SIMD checksum
  paths, which stay reachable via the crc32/adler32 runtime dispatch.
- Coupling is already clean: decompress's non-test code imports only `crate::checksum` +
  `crate::error` (all Compressor refs in decompress files are `#[cfg(test)]`). archmage appears in
  exactly 3 files: checksum/adler32.rs + checksum/crc32.rs (SIMD tiers; scalar impls are plain code
  except a `ScalarToken` param) and matchfinder/mod.rs (compress-only, `autoversion`). libm has
  2 call sites, both `libm::log2` in compress/full_optimal.rs.
- Prototype correctness verified: decoded 587 KB gzip (system gzip -9, CRC-32 verified) and zlib
  (python zlib, Adler-32 verified) fixtures byte-exactly; `cargo check` clean for no_std-no-alloc,
  alloc, alloc+unchecked, and wasm32 decode-only.
- Cleanup surface found: 4 fast_bytes helpers (`load_u32_le`, `store_u64_le`, `get_byte`,
  `prefetch`) become dead in decode-only builds — need `#[cfg(feature = "compress")]`; incant's
  scalar tier needs a tokenless-scalar shim when archmage is optional; matchfinder needs
  `cfg_attr`-style autoversion gating (moot if archmage is only optional for decode-only).
- Feature design sketch: `compress = ["alloc", "dep:libm"]`, `simd = ["dep:archmage"]`, both in
  default; decompress stays unconditional (it's the small part). SEMVER: gating Compressor behind
  `compress` breaks `default-features = false, features = ["alloc"|"std"]` consumers → 0.4.0.
  Known consumers unaffected: zenpng uses defaults; heic + zenzop use default-features=false
  (decode+checksums only today — they'd keep working, dropping to scalar checksums unless they
  add `simd`).
- **IMPLEMENTED in 0.4.0** (2026-07-13): `compress` + `simd` features landed (module-gated:
  checksum SIMD tiers live in `mod simd`, compress-only fast_bytes helpers in
  `mod compress_only`, crate-Compressor tests in gated sibling test mods); libm dropped
  entirely (std → f64::log2, no_std → `log2_series`, ~1e-8 bits error, accuracy-tested).
  Measured on the real crate: decode-only cold build 0.28s debug / 0.30s release,
  dep tree = enough only, 105 decode-only lib tests (libdeflater-compressed decode
  tests stay live). Default features unchanged: 3.2s cold, full API, 245 tests.

### `avx512` stays DEFAULT-ON — measured, do not re-litigate opt-in (2026-07-14)

Considered making `avx512` opt-in for "dep weight." Measured on 7950X (Zen 4,
native AVX-512+VNNI+VPCLMULQDQ), no `target-cpu=native`. Full data:
`benchmarks/avx512_checksum_ab_2026-07-13.md`.

- `avx512` gates ONLY the 512-bit checksum tiers (`adler32_impl_v4x` VNNI,
  `crc32_impl_v4x` VPCLMULQDQ). Nothing in compress/decompress core uses it.
- **Cost of keeping it on is ~nil:** +0 crates (13→13 — `archmage/avx512` only
  toggles codegen inside archmage, already a dep), +0.02s (+1%) cold build,
  +7 KB binary, +0 MSRV (archmage itself requires 1.89 via `simd`, so opt-in
  would NOT lower the default MSRV).
- **Benefit is real but niche:** standalone CRC-32 **4.3×** (18→78 GiB/s),
  Adler-32 1.1–1.6×. gzip pipeline: 0% compress, 1% typical / 10% best-case
  (xml) decompress. The 4× only matters if `zenflate::crc32`/`adler32` is used
  as a standalone checksum library.
- **Decision (user, 2026-07-14):** keep `avx512` in default. Opt-in removes
  nothing measurable and costs 4.3× standalone CRC. Don't revisit without new data.

## Known Bugs

(none currently)

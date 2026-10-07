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
- [x] Phase 11 (unreleased): `CompressionLevel::png(effort)` (src/compress/png_mode.rs,
  src/compress/png_ultra.rs). png(1) = ultra-fast (literals + zero runs, one table per
  stream, counted from the whole input up to 64 KiB, sampled above; pair LUT from
  512 KiB); png(2) runs-only;
  png(3) hash min match 8; png(4..9) min match 5, chains; png(10..18) lazy (libdeflate
  5/6/7 settings, then deeper, no good_match/max_lazy shortcuts) + runs-only guard
  (`RunsGuard`); png(19..22) = new(23)'s near-optimal parser; png(23..) = new(23..).
  Skip-ahead step capped at 256. Validated on 146-150 held-out K300 reps at 3 sizes:
  `benchmarks/png_mode_2026-10-06.md`, tooling in `benchmarks/harnesses/png-mode/validation/`.
- [x] Phase 12 (unreleased): `zenflate::png::{StripCompressor, StripDecoder}` for iDOT
  (src/png/). Shape set by the user (2026-10-06): PNG-specific types, no new free functions
  ("free functions increasing is a red flag"), and no public PNG-only `StreamDecompressor`
  methods. The raw layer (`Compressor::deflate_compress_segment`,
  `StreamDecompressor::with_segment_end` / `zlib_continuation` / `ended_at_segment_boundary`
  / `running_checksum` / `footer_checksum`) is `pub(crate)`; `StripDecoder` wraps it.
  zenpng main (src/decoder/idot.rs) decodes iDOT strips in parallel through this, streaming
  each strip across IDAT chunks and un-filtering row by row, so the decoder must stay
  streaming. Caller-driven, no built-in threads: a threaded helper forced whole-image
  buffering and blocked zenpng's per-strip filter search on its own pool.

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

### PNG mode design notes (2026-10-06, `benchmarks/png_mode_2026-10-06.md`)

- Aggregate Pareto picks hid per-image inversions of up to 10% (runs-only → hash on
  flat-colour clipart). Always check per-image monotonicity across effort ladders,
  not just corpus totals; `png-mode` harness + `ladder.py` do this.
- A greedy bit-cost model (reject matches dearer than their literals) did NOT fix it,
  cost 15-25% speed, and its order-0 first-block estimate hurt small inputs. Removed.
- Demoting hashed matches to literals is not a proxy for the runs-only parse: hashed
  matches swallow run starts. Only a real runs-only parse catches it (the guard).
- Changing min match between rungs causes inversions; keep knobs monotone.
- Center crops at 256x256 are denser than 1 MP crops; deep chains were 3-4x slower per
  byte there until backward extension went forward-first + 8-byte compares.
- fdeflate main (a713d02) L3 is larger than its L2 on 13/35 1 MP filtered images (up to 2.14%).
- Tuning-set ladders don't survive held-out data unchanged: the 150-image held-out check
  (K300 reps, validate+test splits, minus tuning images) found a skip-ahead runaway bug
  (uncapped step jumped whole compressible regions; fixed with a 256-byte cap), showed the
  deep chain levels dominated by new(13..), and that ultra's sampled table loses to
  runs-only on 64x64 inputs. Validate ladders with paired per-image stats (mean +- CI,
  inversion counts) on held-out data before shipping.
- Ultra tables (2026-10-06, `tables/` in the png-mode harness): fdeflate's fixed table is
  near the best single table for filtered PNG (a trained one: -0.9%, per channel count:
  -1.1%, modeled on held-out streams under 64 KiB). Per-image counted tables are -12.9%;
  codebooks of 4-16 tables picked from a 2 KiB prefix get only -5 to -7%. Counted tables
  need priors only where the count can miss a symbol (literal 0, EOB, lengths): giving
  every symbol a code inflates the header (+2.6-4.4% at 64x64).
- Ultra stored fallback must compare end positions in bits, including the extra header
  byte write_uncompressed adds when pending bits spill over: the old byte check let a
  block land 1-2 bytes past zlib_compress_bound (tight below 5000 bytes).
- Without the runs-only guard, even long-match-only (12-24+ byte) hash levels make
  34-48/146 images larger than runs-only (up to 12%): any offset code beyond distance 1
  lengthens the run codes. The guard is required for every hash level.
- Min match 8 -> 5 is a real strategy boundary (27-38 images larger, up to 6.5%); keep it a
  single switch with a monotonicity_fallback, don't step through 7 and 6.

### Inflate on PNG streams (2026-10-06, `examples/png_inflate.rs`)

Measured on zenpng's 106 vs_png inputs (each PNG's IDAT stream), median time
per image, interleaved arms. zenpng's ST decode is ~45% inflate and its
two-thread decode pipeline is bounded by inflate, so this is zenpng's lever.

- **Double-literal litlen entries** (fdeflate's trick): `add_double_literals`
  packs two literals into one 11-bit-table entry when both codewords fit.
  Literal-heavy PNG streams halve their lookups. The slow paths must test
  `HUFFDEC_LITERAL` before any flag (the second literal overlaps bits 8-15).
- **12-bit tables measured no better** with doubles (median 1.045 vs 1.036 of
  fdeflate's time on i265) and slower on small images. Kept 11.
- **The pass is gated** (`DOUBLE_LITERAL_MIN_INPUT` = 16 KiB of compressed input
  left; streaming upgrades the table mid-block once 16 KiB is staged). Ungated,
  64 px images decoded 10% slower on Neoverse-N1.
- **Blocks without doubles keep a single-store literal path** (`put_lits`): the
  two-store `store_lits` makes the output position depend on the loaded entry,
  and alone cost 64 px images +7.7% one-shot on Neoverse.
- **Streaming:** 512-byte input staging made the fastloop exit every ~480
  compressed bytes (32 KiB: 1.167 -> 1.129 of fdeflate's time on i265); per-literal saturating
  `lookback_valid` updates replaced by a fixed `real_start` per fastloop entry
  (1.129 -> 1.092 of fdeflate's time); the 32 KiB lookback window is allocated only when output outgrows
  `capacity` (zenpng measured its zero-fill at ~26K of ~620K instructions in a
  64x48 decode).
- Results (new/base, median per image): x86 265K one-shot 0.972, streaming
  0.966; Neoverse-N1 one-shot 0.980, streaming 0.984; M4 Pro one-shot 0.970,
  streaming 0.945. A 32 KiB *initial* staging buffer cost 64 px streams 4% on
  x86 (zeroing); it now starts at 4 KiB and doubles while the source fills it. Data: `~/tmp` runs on i265,
  arm-big and mac (not committed; the CHANGELOG entry carries the summary).
- On ARM zenflate was already faster than fdeflate one-shot (0.90x); on x86 it
  was 1.11x slower one-shot and 1.18x streaming before this work.
- **16-byte chunked match copy** (fdeflate's): the fastloop copies matches as
  fixed 16-byte `copy_within`s (no memmove call per match; offset 2..=15 step
  by the offset), margin +16. The biggest single x86 win: PNG one-shot 1.035 ->
  0.97 of fdeflate's time, streaming 1.066 -> 1.007; Silesia/Canterbury one-shot
  and streaming ~0.89 of fdeflate. Rejected variants: exact memmove for
  length >= 64 non-overlapping, and period-doubling for small offsets - both
  slower overall and neither fixed `nci` one-shot on Neoverse-N1 (+7% vs
  before the chunked copy, cause unknown; its streaming decode got faster).
- **One-shot decode may overwrite `output` past `output_written`** (up to ~16
  bytes, only when the stream ends inside the fastloop, i.e. the buffer has
  more than ~278 bytes of slack; exactly sized buffers are unaffected). Kept on
  purpose (user, 2026-10-07: "speed"): exact tail copies would bring back a
  variable-length copy on most PNG matches (< 16 bytes), the cost the chunked
  copy removed. Documented on deflate/zlib/gzip_decompress; fdeflate's read()
  documents the same. Don't "fix" it without re-asking.
- **x86-64-v3 build of the streaming loop** (archmage `#[arcane]` X64V3Token):
  streaming 1.105 -> 1.066 of fdeflate on the 265K. The same for the one-shot
  core was 1.8-2.5% slower, and so were full-width 11-bit tables with a
  constant mask, 12-bit tables, and bounds-check-free typed table lookups
  (LLVM's checks evidently help its codegen here). Measure; don't assume.
- Consumers checked against these changes (2026-10-07, copies in ~/tmp with a
  path dep): heic (`unci`, one-shot, default-features=false: 32 suites incl. 36
  unci tests), zenzop (checksums only), zensim-validate - all pass. No-`simd`
  builds (heic's config) decode zlib ~6% slower than fdeflate because Adler-32
  is scalar there; enabling `simd` in the consumer fixes that.
- `fuzz_inflate_diff` (one-shot vs streaming vs miniz_oxide) is the correctness
  gate for decoder changes; the old `fuzz_decompress` only catches crashes.
  Under `cfg(test)`/`cfg(fuzzing)` the doubles threshold is 0.

### PNG ladder on identical filtered bytes (2026-10-06)

`examples/png_ladder_pareto.rs`, `benchmarks/png_ladder_pareto_{arm,mac}_2026-10-06.txt`,
`benchmarks/png_ladder_lazy_guard_2026-10-06.txt` (after the png(10..) change):

- zenflate beats miniz_oxide (image-png's balanced/high codec) at every size
  point: e15 3.864 @ 45 MB/s vs miniz 6 3.849 @ 20 (Neoverse); png(1) 3.08 vs
  fdeflate ultra-fast 2.84 at 1.6-2x its speed.
- `new(1..=9)` are dominated by `png()` on PNG data (e1-e4 identical bytes,
  e5-e9 identical).
- Gap (fixed the same day): libdeflate 5/6 beat the old png(10..=12) and e12/e13 in
  the 60-80 MB/s band (Neoverse). One-step lazy matching in png(10..=12) gave -0.5% size for
  +32-42% time and stayed dominated: rejected.
- What worked: libdeflate's lazy parser (3-byte matches) plus the runs-only guard,
  without new()'s good_match/max_lazy shortcuts (those cost ~1.2% at equal speed:
  e11 3.756 vs libdeflate 5 3.800). Guarded rungs are never larger per image than
  libdeflate 5/6/7. The guard costs ~30% time at png(10).
- Double-lazy (Lazy2) was worse than plain lazy at equal depth on PNG data
  (png(15) Lazy2 depth 200 lost to png(14) lazy depth 200 on 29/86 images), and
  deep lazy (450) beat every Lazy2 rung: png(10..=18) are all lazy.
- Below ~10 MB/s near-optimal parsing beats any lazy depth (libdeflate 10 4.120
  @ 7 MB/s vs lazy depth 3000 3.976 @ 7): png(19..=22) use new(23)'s parser
  (`CompressionLevel::near_optimal_effort` keeps their output identical to
  new(23) while `effort()` reports 19-22).
- Block-split butterfly: lazy depth 295 -> 299 made 8007_rgb8_256 7.5% larger:
  a 5-byte shift of the first boundary (18674 vs 18679) cascades into different
  splits for the rest of the stream (libdeflate's statistics-based
  `should_end_block`). Measured alternatives on the 86 inputs (png(10..=18)):
  no early block ends: +0.6% total, up to +15% on 48/86 images, but rung
  inversions drop to 0.04%; fixed 64 KiB blocks: +0.24% total (median +0.06%),
  up to +9% on images with mid-stream content changes, worst inversion 0.36%
  (32 KiB and 128 KiB were worse). Kept adaptive splitting. Idea not yet tried:
  split points from the input alone (e.g. byte-histogram change points on the
  filtered rows), computed once and shared by every rung and the runs-only
  guard - adaptive and parse-independent, so rungs couldn't diverge.
  IMPLEMENTED for png(10..=18) (`block_split::input_block_end`, zenpng asked
  for monotone rungs): splitting on the runs-only tokenization of the input (a
  5+ byte run = one match observation, other bytes literals) costs +0.12% total
  vs parse-driven splitting (raw bytes as literals cost +0.55%, residual-
  magnitude buckets +0.61%); neighbour inversions from png(12) up are <= 0.07%
  (png(11)/png(13) 0.6% on the 16-bit image). Speed cost on Neoverse:
  png(10) +14%, png(12) +9%, png(14) +5%, png(16..=18) +4% (about half is the
  scan at ~18 instructions/byte after unchecked-free tightening, the rest
  different block sizes). Blocks are capped at 3 * (SEQ_STORE_LENGTH - 1)
  bytes so the sequence store can't end one early (a parse-dependent end).
  Matches must not cross the shared block end, and `adjust_max_and_nice_len`
  only lowers max_len/nice_len, so they are reset per block (forgetting that
  made output 12-16% larger). On 5207_rgb16_1024 greedy min-match-5 png(9)
  beats every lazy rung (3-byte lazy matches suit 16-bit samples poorly). new() keeps
  parse-driven splitting.
- Before the guard, e13 was larger than png(12) on 41/86 images (up to 8.6%).

### Strategy state must survive early returns (2026-10-06)

Each strategy takes its matchfinder / near-optimal state out of `Compressor`
(`self.x.take()`) so it can call `&mut self` helpers, and used to put it back
only on the success path. An early return (a `Stop` firing; output overflow is
reported after the strategy returns, so it never triggered this) left the field
`None`, so the compressor's next call panicked on `unwrap()` (verified at
`new(1..=30)` and `libdeflate(1..=12)` on 844cb8a).
Fixed by splitting each strategy into a wrapper that always restores the state
and an `_inner` body that receives it. Any new strategy that takes state out
must do the same. Guarded by `tests/conformance.rs` (`recovery_*`: stop after
0-20 checks, buffers from 0 bytes to one short, then reuse must match a fresh
compressor) and the cross-API matrix (`just conformance-full` in release).

## Known Bugs

(none currently)

# Changelog

## [Unreleased]

### Fixed

- **Pushes to `main` now cancel their superseded CI runs.** `ci.yml` and
  `miri.yml` keyed their concurrency group on
  `${{ github.head_ref || github.run_id }}`. `github.head_ref` is populated only
  for `pull_request` events, so on a push it was empty and the group fell through
  to `github.run_id` — unique per run, so no two runs ever shared a group and
  `cancel-in-progress` could never fire. Now keyed on `${{ github.ref }}`, which
  is set for every trigger these workflows use. PR cancellation is unchanged;
  consecutive pushes to a branch now supersede each other. `miri.yml` runs only
  on `v*` tags and `workflow_dispatch`, where `github.ref` is the tag ref (unique
  per tag, so release runs still never cancel one another) or the dispatched
  branch ref (so a re-dispatch supersedes the run it replaces).
- **The `Fuzz regression` CI job could not fail, and replayed nothing.** It ran
  `cargo test --test fuzz_regression 2>/dev/null || echo "No regression test
  found…"` inside an `if [ -d fuzz/regression ]` guard. Three separate defects
  stacked: `|| echo` swallowed the exit status of a genuinely failing suite,
  `2>/dev/null` hid the reason, and the `ls | wc -l` count matched on
  `README.md` alone, so the branch was entered and the suite ran over zero
  seeds. `tests/fuzz_regression.rs` has existed the whole time, so the "no test
  harness" fallback was masking real failures rather than covering a missing
  target. The step is now a bare `cargo test --test fuzz_regression`. The
  harness pins the corpus size at `EXPECTED_SEEDS`, which is **deliberately 0
  here** and documented in place: #7 needs a ~19 MB input and is gated by the
  unit test in `src/compress/full_optimal.rs` instead of a committed seed, so
  zenflate's empty corpus is now a visible decision rather than an accident.

- **The first version of that corpus guard (`c0211d6d`) shipped an assertion
  that could not fail**, which is the same defect it was written to remove. It
  read `assert!(found >= MIN_SEEDS)` with `MIN_SEEDS = 0` — true for every
  `usize` that can exist. Stable clippy rejected it
  (`absurd_extreme_comparisons`, "because `MIN_SEEDS` is the minimum value for
  this type, this comparison is always true"), which broke the `Clippy` job and
  is how it was caught. The vacuity was not only theoretical: with the guard as
  written, **emptying `fuzz/regression/` left the suite green** — the directory
  check still saw a directory and `0 >= 0` still held. The bound is now an
  equality against the committed count, plus an explicit check for the corpus
  `README.md` (the only tracked file in that directory, and so the only reason
  git materialises it at all). Mutation-verified, each run to completion:
  renaming the directory away, emptying it, and adding one unaccounted-for seed
  each fail with a distinct message (exit 101); restoring passes. The pre-fix
  harness was re-run against the emptied-directory mutation to confirm it
  passed — the hole was real, not hypothetical.

- **FullOptimal (Zopfli) could silently emit a corrupt DEFLATE stream when a
  single block contained a literal run of 2^23+ items** (issue #7, found by
  the 2026-08-26 cross-codec ultracode sweep, adversarially verified). The
  run count is packed into the low 23 bits of
  `Sequence::litrunlen_and_length`; an overflowing run (matchless data — e.g.
  unique-trigram content — after a compressible prefix keeps the dynamic
  arm selected) bled into the 9-bit length field, and the garbage length hit
  codeword slots the Huffman tables never filled, emitted as ZERO bits.
  `flush_lz77_block` now subdivides oversized stores into multiple DEFLATE
  blocks at the leaf (2^22-item cap; one extra block header per 4M items),
  and the Sequences emit arm `debug_assert`s every match length is in the
  DEFLATE range with a nonzero codeword. Regression test drives the flush
  directly with a 10.3 MB match prefix + 8.4M-literal run and roundtrips;
  mutation-verified (fails with `BadData` when the cap is disabled).

## [0.4.0] - 2026-07-14

### Changed

- **BREAKING (0.4.0):** compression is now behind the `compress` feature
  (on by default, implies `alloc`). `default-features = false` consumers that
  used `Compressor`/`CompressionLevel` via `alloc`/`std` must add `compress`;
  benches and examples declare `required-features = ["compress"]` (ba343a6).
- **BREAKING (0.4.0):** SIMD is now behind the `simd` feature (on by default);
  archmage is an optional dependency. Without `simd`, Adler-32/CRC-32 use the
  scalar paths (identical output, verified by the C parity suite) and the
  matchfinder init/rebase helpers drop `#[autoversion]` multiversioning.
  `avx512` now implies `simd`; `threads` now implies `compress` (a720edf).
- Compress-only code is module-gated rather than attr-gated: checksum SIMD
  tiers live in `mod simd`, compress-only byte helpers in
  `fast_bytes::compress_only`, and Compressor-dependent decompression tests in
  gated sibling test modules. Decompression tests that compress via libdeflater
  (C) now run in decode-only builds — 105 decode-only lib tests, up from a
  would-have-been 72 (645fdaf).
- Docs: effort range corrected to 0-200 everywhere (31-200 = Zopfli-style
  FullOptimal, `iterations = effort − 16`); README/lib.rs gained a feature
  table, decode-only build guide, and 0.3→0.4 migration note.
- `CompressionError`, `DecompressionError`, and `StreamError` now implement
  [`core::error::Error`] instead of `std::error::Error` (a re-export of it
  since Rust 1.81; MSRV is 1.89). The two whole-buffer errors implement it
  **unconditionally** — available in `no_std` builds, not just under `std`;
  `StreamError`'s impl tracks its own `alloc` gate. The `std` feature now
  gates only the `std::io::{Read, BufRead}` integration (`BufReadSource`).
  Additive — `cargo semver-checks` reports no new break (220eb99).

### Removed

- **BREAKING (0.4.0):** the `libm` dependency. `std` builds use `f64::log2`;
  `no_std` builds use a local deterministic `log2_series` (exact exponent +
  atanh-series mantissa, ~1e-8 bits absolute error, accuracy-tested against
  `f64::log2`). Only call sites were the Zopfli entropy estimate's two log2
  calls — cost-model setup, not a hot loop (9f22a14).

### Fixed

- README streaming-decompression example used non-existent constructors
  (`StreamDecompressor::new_deflate`/`new_gzip`) and a non-idiomatic loop. It
  now matches the real API: `StreamDecompressor::deflate(source, DEFAULT_CAPACITY)`
  (and `gzip`/`zlib`) driven by `while !is_done() { fill()?; peek(); advance(n) }`,
  plus a new untrusted-input note on `with_max_output_size` bomb defense. Docs only.
- Two `DecompressionError::OutputLimitExceeded` intra-doc links
  (`Decompressor`/`StreamDecompressor::with_max_output_size`) were bare and
  unresolvable from `error.rs`'s scope; now fully-qualified `crate::` targets so
  docs.rs renders them. `cargo doc` is clean under `-D warnings` (220eb99).

### Changed (earlier, docs/infra)

- README/onboarding refresh: standardized badge row (CI/crates.io/lib.rs/docs.rs/MSRV/license, all `flat-square`, no `branch=`), added a copy-paste `## Quick start` round-trip, documented the `threads` feature, wrapped the heavy benchmark tables in `crates.io:skip` markers behind an honest v0.3.1-provenance note, refreshed the crosslink footer, and made License links absolute. crates.io now ships a generated `README.crates.md` (`readme = "README.crates.md"`); added `benchmarks/README.md` (methodology + repro). Docs only.
- Trimmed `.gitignore`, `UPSTREAM-AUDIT.md`, `benches/`, and `tests/` from the published package tarball via `exclude`; no behaviour change.

- `tests/fuzz_regression.rs` now uses the shared `zen-fuzz-regress`
  test-helper crate (DEDUP-J2). Behaviour is unchanged — same
  `fuzz/regression/` seeds, same `Decompressor::{deflate,zlib,gzip}_decompress`
  entry points, same panic-propagation failure semantics. The
  ~50-line in-file scaffolding (`collect_seeds`, walk + skip dotfiles
  + skip `README.md`) is now provided by `RegressionSuite`.

### Added

- Versioned public-API surface snapshot at `docs/public-api/zenflate.txt`,
  regenerated by `tests/public_api_doc.rs` on every `cargo test` run
  (`ZEN_API_DOC=check` verifies in CI's clippy job, `=off` skips elsewhere);
  `just api-doc` / `just api-doc-check` recipes added.

- `tests/fuzz_regression.rs` regression-harness template ported from
  zenwebp (DEDUP-J). Walks `fuzz/regression/` (incl. per-target subdirs)
  and runs every raw-bytes seed through the `Decompressor::{deflate,
  zlib, gzip}_decompress` entry points covered by the `fuzz_decompress`
  fuzz target on the stable toolchain — no nightly required. Created
  `fuzz/regression/README.md` documenting how to add minimized crash
  seeds (and how to extend the harness for `fuzz_roundtrip`'s
  arbitrary-encoded seeds).

## 0.3.2 (2026-03-25)

### Fixed
- **Stack overflow in `Compressor::clone()` with `unchecked` feature.** `NearOptimalState` (~9MB of fixed arrays) was placed on the stack by the derived `Clone`, overflowing the default 8MB thread stack. Downstream crates (zenpng) hit this in beam search filter evaluation.

### Changed
- `NearOptimalState` and `BtMatchfinder` now use `Vec` for large tables regardless of feature flags. The `unchecked` feature controls access patterns (`get_unchecked`, raw pointers), not storage layout. This eliminates the `Box::new_uninit()` construction path and all associated unsafe code.
- Performance benchmarks in the README were measured with 0.3.1. The Vec-based layout may show different performance characteristics at NearOptimal levels (10-12) and should be re-benchmarked.

## 0.3.1 (2026-03-25)

### Fixed
- Cooperative `Stop` checking granularity improved to <10ms in all compression strategy inner loops (was unbounded)
- Added `scalar` fallback tier to `incant!` dispatch in Adler-32 and CRC-32, fixing archmage deprecation warnings

### Changed
- Bumped `archmage` 0.9.9 → 0.9.12
- Bumped `enough` 0.4 → 0.4.2
- Bumped `libm` 0.2 → 0.2.16

### Added
- Corpus benchmarks (Canterbury, Silesia, photos) with fresh measurements and safe-mode columns

## 0.3.0

### Added
- **FullOptimal compression** (Zopfli-style iterative optimization)
  - Katajainen bounded package-merge for optimal Huffman codes
  - Accurate block cost estimation with early-exit max-bits sweep
  - Chunked prefix-sum histograms for O(64) block split cost
  - Lightweight `block_cost_simple` for fast block splitting decisions
  - Squeeze optimizations ported from zenzop
  - Configurable iteration count per effort level
- **Enhanced Huffman optimization**
  - Multi-strategy Huffman code optimization (A2)
  - Exhaustive precode tree header search (A1)
  - Near-optimal parser diversification (A3)
  - `optimize_huffman_for_rle` functions (Brotli-inspired + Zopfli-style)
- `CompressorSnapshot` and cost estimation for incremental API
- `#[must_use]`, `#[non_exhaustive]`, and `Debug` impls on public types
- Byte-identical parity tests for all libdeflate compat levels (0-12) across
  deflate, gzip, and zlib formats with multiple data patterns
- README badges, MSRV section, and AI disclosure

### Fixed
- L1 `compress_fastest`: `next_hash` not persisted across block boundaries,
  causing different output than C libdeflate on multi-block inputs
- gzip header XFL byte not set based on compression level (should be 0x04 for
  fastest, 0x02 for best, matching C libdeflate)
- zlib header FLEVEL mapped level 7 to SLOWEST instead of DEFAULT
- Lazy2 off-by-one in incremental compression skip count
- Hash update guard against OOB in greedy match skip loops
- Swap (dist, length) -> (length, dist) return order from match loop
- `fuse_7` precode encoding counted 8 positions instead of 7
- ECT optimizations suppressed in libdeflate compat mode
- `no_std + alloc` compilation: `f64::log2()` in full-optimal replaced with
  `libm::log2`, unused imports removed

### Changed
- Project description updated: no longer described as "a port of libdeflate"
  but as its own implementation with credited origins
- Module-level doc comments updated to distinguish ported core from extensions
- Removed PNG cost bias from core zenflate (moved to codec layer)
- Reuse `HuffmanScratch` in block splitting, use `FnMut`
- Edition 2024, MSRV 1.89
- Bumped `safe_unaligned_simd` minimum to 0.2.5
- Updated archmage/magetypes to 0.9
- Added `libm` dependency for `no_std` floating-point math
- Removed `safe_unaligned_simd` direct dependency (use archmage prelude re-exports)

## 0.2.1

Fix aarch64 stable compilation (removed nightly-only intrinsics).

## 0.2.0

Initial release.

### Compression (`src/compress/`)
- Bitstream writer, Huffman construction, block flushing (`bitstream.rs`,
  `huffman.rs`, `block.rs`, `sequences.rs`) — ported from libdeflate
- Block splitting with 10-category observation system (`block_split.rs`)
  — ported from libdeflate
- Level 1: `compress_fastest` with ht_matchfinder (`mod.rs`,
  `matchfinder/ht.rs`) — ported from libdeflate
- Levels 2-9: greedy, lazy, lazy2 strategies with hc_matchfinder
  (`mod.rs`, `matchfinder/hc.rs`) — ported from libdeflate
- Levels 10-12: near-optimal parsing with bt_matchfinder
  (`near_optimal.rs`, `matchfinder/bt.rs`) — ported from libdeflate
- Effort-based `CompressionLevel` (0-30) with six strategies and named
  presets replacing libdeflate's fixed 0-12 levels
- Turbo matchfinder (`matchfinder/turbo.rs`) — original, single-entry
  hash with limited skip updates for efforts 1-4
- FastHt matchfinder (`matchfinder/fast_ht.rs`) — original, 2-entry hash
  with limited skip updates for efforts 5-7
- `good_match`/`max_lazy` early-out optimizations
- Parallel gzip compression with pigz-style chunking, 32KB dictionary
  overlap, and CRC-32 combine via GF(2) matrix
- `Clone` for `Compressor` + incremental compression API

### Decompression (`src/decompress/`)
- Core decompressor with decode tables and fastloop (`mod.rs`) — ported
  from libdeflate's `deflate_decompress.c` and `decompress_template.h`
- gzip/zlib wrapper handling with DEFLATE/zlib/gzip format support
- Optimized match copy in fastloop
- `skip_checksum` flag for skipping verification
- Streaming decompression (`streaming.rs`) — original, pull-based API
  with `InputSource` trait, works in `no_std + alloc`
- `BufReadSource` for `std::io::BufRead` integration

### Checksums (`src/checksum/`)
- Adler-32 scalar — ported from libdeflate's `adler32.c`
- Adler-32 SIMD: AVX-512 VNNI, AVX-512, AVX2, NEON, WASM simd128
  — original implementations via archmage
- CRC-32 scalar + slice-by-8 — ported from libdeflate's `crc32.c`
- CRC-32 SIMD: PCLMULQDQ 128-bit, VPCLMULQDQ 512-bit, aarch64 PMULL
  — folding constants from libdeflate, implementations via archmage
- `adler32_combine` and `crc32_combine` for parallel checksum merging
- `Adler32Hasher` and `Crc32Hasher` wrapper structs

### Infrastructure
- `#![forbid(unsafe_code)]` by default, opt-in `unchecked` feature
- `no_std` + `alloc` support (decompression fully stack-allocated)
- `enough` crate integration for `Stop` / cancellation trait
- Criterion benchmarks vs libdeflate, flate2, miniz_oxide, fdeflate, zlib-rs
- GitHub Actions CI with x86_64, i686, aarch64, WASM targets
- Miri CI for unsafe soundness checking
- cargo-fuzz infrastructure
- Justfile and Dockerfile

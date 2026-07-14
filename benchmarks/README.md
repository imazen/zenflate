# zenflate benchmarks — methodology & reproduction

How to run zenflate's comparisons fairly, and how to read the committed result
files in this directory. The numbers in result files are tied to a specific
commit, host, and command (recorded in each file's header) — reproduce against
that commit, not `main`.

## Fairness guarantees

The comparisons are built so the numbers mean something:

- **Interleaved (paired) measurement.** The `throughput` bench runs on
  [zenbench](https://github.com/imazen/zenbench)'s criterion-compat harness,
  which interleaves contenders (A,B,A,B…) rather than "all of A, then all of B."
  Both libraries see the same thermal state, turbo residency, and OS scheduling,
  so the paired difference cancels systematic drift.
- **No I/O in the timed region.** Inputs are generated (synthetic) or read from
  the corpus cache into `Vec<u8>` *before* timing starts; the timed closure only
  calls compress/decompress on the in-RAM buffer. No file open/read/write is
  measured. Output is consumed so it isn't optimized away.
- **Single-thread vs single-thread.** The core compress/decompress comparison is
  one call on the calling thread for every contender. Parallel gzip
  (`gzip_compress_parallel`) is measured separately and labelled with its thread
  count — never compare a thread-pooled run against a single-threaded one.
- **No `-C target-cpu=native`.** All builds use runtime SIMD dispatch (archmage
  `incant!`), which is what ships. Native builds bake in ISA extensions and give
  misleading numbers.
- **Apples-to-apples inputs.** Same bytes, same format (raw DEFLATE or zlib, as
  noted per file), and levels compared at matched *ratio* — not just matched
  level number — where the level scales differ (see `zenflate_vs_zlibrs_2026-06-13.md`).
- **`safe` vs `unchecked` stated explicitly.** zenflate's default build is
  `#![forbid(unsafe_code)]` (`safe`); `--features unchecked` opts into
  bounds-check elision in compression hot paths. Each result file says which it
  used. Note that zlib-rs has no safe-only mode, so a default-feature zlib-rs is
  the fair comparison point (documented in `image_deflate_safe_vs_unchecked_2026-06-18.txt`).

## Competitor versions

Pinned as dev-dependencies, so `cargo` resolves them for you. The versions used
for the committed result files (pin these if reproducing elsewhere):

| Competitor | Version | Notes |
|-----------|---------|-------|
| [`libdeflater`](https://crates.io/crates/libdeflater) | 1.25 | C libdeflate bindings (the reference) |
| [`flate2`](https://crates.io/crates/flate2) | 1.1 | built with the `zlib-rs` backend (≈ zlib-rs, ~zero wrapper overhead) |
| [`zlib-rs`](https://crates.io/crates/zlib-rs) | 0.6 | benched without the opt-in `avx512`/`vpclmulqdq` features |
| [`miniz_oxide`](https://crates.io/crates/miniz_oxide) | 0.9 | direct |
| [`fdeflate`](https://crates.io/crates/fdeflate) | 0.3 | decompression comparison |

## Reproduce

```sh
git clone https://github.com/imazen/zenflate && cd zenflate
git checkout <commit>          # the commit named in the result file you're reproducing

# Interleaved synthetic throughput vs the whole ecosystem (1 MB synthetic):
cargo bench --bench throughput --features unchecked

# Standard corpora (Canterbury / Silesia / gb82 photos), throughput + ratio:
cargo bench --bench corpus

# Real-corpus ratio + wall-clock vs zlib-rs (compress & decompress):
cargo run --release --features unchecked --example zlib_rs_ratio

# Image-DEFLATE corpus (PNG/TIFF residual byte streams):
IMG_PER_CLASS=4 cargo run --release --features unchecked --example image_deflate_corpus
```

The `corpus` bench expects files under `~/.cache/compression-corpus/`
(Canterbury, Silesia) and `~/.cache/codec-corpus/v1/gb82/`; override the location
with `COMPRESSION_CORPUS_CACHE`. Build **without** `-C target-cpu=native`.

## Result files

Each committed run records its git commit, host, date, and exact command in the
file header. Current files:

- `zenflate_vs_zlibrs_2026-06-13.md` — zenflate 0.3.6 vs zlib-rs 0.6, raw DEFLATE,
  compared at **matched ratio** (the headline frontier comparison).
- `throughput_interleaved_2026-06-18.txt` — single interleaved zenbench pass,
  zenflate vs zlib-rs / flate2 / libdeflate / miniz_oxide on 1 MB synthetic data.
- `zlib_rs_compare_2026-06-18.txt` — `zlib_rs_ratio` example output: ratio +
  throughput, compress & decompress, on real corpora + synthetic.
- `image_deflate_corpus_2026-06-18.txt` — zenflate vs zlib-rs on PNG/TIFF
  predictor/filter residual streams (the imazen-26 PNG set).
- `image_deflate_safe_vs_unchecked_2026-06-18.txt` — the same image-DEFLATE
  corpus, `safe` (default) vs `unchecked`, with the zlib-rs fairness note.
- `deflate_rust_ecosystem_2026-07-13.md` — zenflate 0.4.0 vs the full Rust
  ecosystem (libdeflater, flate2/zlib-rs, miniz_oxide, fdeflate, libflate, yazi,
  zune-inflate), per-host on Hetzner train-1 (CCX63 x86) and arm-big (CAX31
  aarch64) plus WSL2, with a fast-end dickens reality check.
- `rd_sweep_train1_2026-07-13.csv` — 120 roundtrip-verified rate/distortion
  points (every library × level: size, adaptive median time, zenflate decode)
  from the CCX63 box; backs `examples/rd_sweep.rs`.
- `avx512_checksum_ab_2026-07-13.md` — does the `avx512` feature earn default-on?
  A/B of the 512-bit checksum tiers (v4x) vs AVX2 (v3): standalone CRC-32 4.3×,
  Adler-32 1.1–1.6×, but gzip pipeline only 0–10%; +0 crates / +1% build / +0 MSRV
  to keep on. Raw 5-round data + harness sources in `harnesses/`.

Do not commit numbers you didn't generate, and don't extrapolate one size or
machine to another — measure each. Memory claims need heaptrack / `time -v`, not
estimates.

## Charts (what to plot for which decision)

| Question | Chart |
|----------|-------|
| "Which library is fastest?" | horizontal **bar**, sorted by throughput (MiB/s); single-thread bars only, unless a parallel series is explicitly labelled |
| "Speed vs ratio across levels?" | **scatter / line**: x = ratio (or bytes), y = throughput, one line per library swept across levels; compare at matched ratio, not matched level number |
| "Is the A/B delta real / how noisy?" | **violin** or PDF of per-call times, or the paired CI the interleaved zenbench run prints |
| "How does it scale with input size?" | **line**, x = bytes (log); fit `total = α + β·bytes` and report both the fixed overhead and the per-byte slope |

For new comparison charts, prefer [zenbench](https://github.com/imazen/zenbench)
— it does the interleaving for you and emits a sorted throughput bar chart, a
self-contained SVG report (`--format=html`), and violin/PDF/regression plots.

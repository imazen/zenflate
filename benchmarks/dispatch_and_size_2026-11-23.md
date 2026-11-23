# Stop type erasure + matchfinder dispatch, 2026-11-23

Candidate: working copy on baseline `7270e38f` (`chore(deps): upgrade archmage to 0.9.33`).
Change: internal `stop: &impl enough::Stop` parameters erased to `Option<&dyn enough::Stop>`
across compress strategy internals, `deflate_decompress_core*`/`oneshot_v4::core_v4`, and
`scan_core`; public entry points (`Compressor::{deflate,zlib,gzip}_compress`,
`Decompressor::{deflate,zlib,gzip}_decompress`, `deflate_scan`/`zlib_scan`) keep
`stop: impl enough::Stop` and bind `let stop = (&stop as &dyn enough::Stop).live();` once
before fan-out. `live()` turns a stop that can never fire (`Unstoppable`) into `None`, so
hot loops pay a branch, not an indirect call; `Option<T: Stop>: Stop` keeps `stop.check()?`
call sites unchanged.
Secondary: `matchfinder_init`/`matchfinder_rebase` `#[autoversion]` → `#[attune(dispatch)]`
(archmage 0.9.33), which drops the V4 variant; these are bandwidth-bound 64–256 KiB
fill/saturating-add loops.

## Stop monomorphization counts (nm -C on release test binaries, i265)

Instantiation counts for `::<...>`-suffixed copies of the named functions
(`nm -C --defined-only`, release builds of `zenflate-<hash>` libtest and
`conformance-<hash>` in each tree):

| Binary / function | BEFORE (7270e38f) | AFTER (candidate) |
| --- | ---: | ---: |
| libtest `compress_full_optimal` | 3 (`Unstoppable`, test `Cancel`, `&CountingStop`) | 0 (non-generic) |
| libtest `compress_lazy_generic` | 3 | 0 |
| libtest `deflate_compress_chunk` | 2 (`Unstoppable`, test `Cancel`) | 0 |
| libtest `compress_{greedy,fastest,fast_ht,near_optimal,turbo,static_turbo,png,png_ultra}` | 1 each (`<Unstoppable>`) | 0 each |
| libtest `deflate_decompress_core` | 1 (`<Unstoppable>`) | 0 |
| conformance `compress_full_optimal` | 2 (`Unstoppable`, `&StopAfter`) | 0 |
| conformance `compress_lazy_generic` | 2 | 0 |
| conformance `compress_{greedy,fastest,fast_ht,near_optimal,turbo,static_turbo,png,png_ultra,deflate_decompress_core,deflate_compress_chunk}` | 1 each | 0 each |

Remaining `::<...>` copies are public-API generics over `impl Stop`
(`{deflate,zlib,gzip}_compress`, `deflate_compress_incremental`,
`deflate_compress_segment_after`, `gzip_compress_parallel`,
`{deflate,zlib,gzip}_decompress`) plus test-side generics (`compress_all`,
`decompress_all`, `stream_decompress_all`) — kept by design; the public signatures are
unchanged.

| Artifact | BEFORE bytes | AFTER bytes | Delta |
| --- | ---: | ---: | ---: |
| libtest binary (`zenflate-…`) | 25,931,496 | 24,622,448 | −1,309,048 (−5.05%) |
| conformance binary | 15,796,032 | 15,166,576 | −629,456 (−3.98%) |
| `tools/wasm-check` .wasm (release, simd+avx512) | 356,085 | 351,032 | −5,053 (−1.42%) |

The wasm-check consumer uses few `Stop` types, so its delta is small; the win scales with
the number of distinct `Stop` instantiations a downstream build would otherwise emit.

## matchfinder dispatch variants (nm -C, libtest)

| Variant set | BEFORE | AFTER |
| --- | --- | --- |
| `matchfinder_init` | `__arcane_..._v3` + `__arcane_..._v4` | `__attune_..._v3` only |
| `matchfinder_rebase` | `__arcane_..._v3` + `__arcane_..._v4` | `__attune_..._v3` only |

## Throughput (i134, i5-13400F, P-core 2)

Runner: `remote_bench.sh i134 zenflate` — `taskset -c 2`, `nice -n 19`, single-writer
flock. Command: `export ZENBENCH_CPU_POLICY=unrestricted; cargo bench --bench throughput;
cargo bench --bench checksum_tiers`. zenbench could not verify the process boundary
(i134's cgroup v2 has no delegated cpuset, so `cpuset.cpus.effective` = 0-15); placement is
`UNVERIFIED_UNRESTRICTED` with `cpu_thread_effective=[2]` — pinning enforced externally by
the runner. Each criterion-compat arm is single-shot ("0 rounds"), so i134 drift of ±10-15%
between runs is visible on the ~130 µs arms; three paired samples were taken for the
compress groups (one reversed-order pair) and all arms' ranges overlap.

zenflate-arm means (µs/ms per call), BEFORE = baseline `7270e38f`, AFTER = final candidate
(`Option<&dyn Stop>` erasure). The definitive pair is the *v2* run measured on the final
tree; earlier samples (marked *) were taken on an intermediate `&dyn Stop` tree and are
listed only to document i134's run-to-run drift.

| Group | Arm | BEFORE v2 | AFTER v2 | Δ |
| --- | --- | ---: | ---: | ---: |
| compress/sequential | L1 | 127.1 | 145.1 | +14.2%* |
| | L2 | 126.5 | 126.7 | +0.2% |
| | L4 | 128.0 | 125.7 | −1.8% |
| | L6 | 140.6 | 140.9 | +0.2% |
| | L9 | 140.5 | 141.2 | +0.5% |
| | L10 | 1645.8 | 1354.1 | −17.7%* |
| | L12 | 1555.5 | 1556.1 | 0.0% |
| compress/zeros | L1 | 127.2 | 126.5 | −0.6% |
| | L2 | 126.3 | 125.6 | −0.6% |
| | L4 | 131.2 | 126.6 | −3.5% |
| | L6 | 140.0 | 140.1 | +0.1% |
| | L9 | 140.7 | 139.6 | −0.8% |
| | L10 | 1393.7 | 1389.8 | −0.3% |
| | L12 | 1434.2 | 1421.1 | −0.9% |
| compress/mixed | L1..L12 | 6.8–10.3 ms | 6.8–10.3 ms | ±0.5% |
| compress/photo | L1..L12 | 7.1–12.5 ms | 7.1–12.6 ms | ±1% |
| decompress | seq/zeros/mixed/photo | 55.2, 67.6 µs, 1.5, 1.7 ms | 52.7, 67.5 µs, 1.5, 1.7 ms | −4.5%, −0.1%, 0, 0 |
| stream_decompress | whole/stream/4k seq | 55.8, 61.5, 85.6 | 51.8, 61.8, 84.8 | −7.2%, +0.5%, −0.9% |
| parallel | 1T/2T/4T | L1 27.6/28.1/28.5, L6 32.6/33.3/33.7, L12 42.0/42.9/43.5 ms | L1 27.4/28.1/27.9, L6 32.4/35.0/34.2, L12 42.3/42.9/43.5 ms | see v3 note |
| adler32+crc32 (throughput) | zenflate | 16.9, 29.6 µs | 17.0, 29.1 µs | +0.6%, −1.7% |
| checksum_tiers | v3(avx2) | 1.8, 2.4, 102.3, 128.4 µs | 1.2, 2.0, 91.2, 132.8 µs | noise (±10%) |

* Outlier note: i134 drifts ±10-15% on the ~130 µs arms between runs. Across earlier
superseded `&dyn Stop` samples plus the final `Option<&dyn Stop>` candidate, L1-sequential
ranges overlap (`{127.1–151.1}` BEFORE vs `{137.7–162.8}` µs AFTER; superseded pairs saw
`−7.7%`, `−4.7%`, `+6.7%`, while final-candidate v2/v3 pairs are `+14.2%`, `+15.8%`, `+0.7%`) —
demonstrating high L1 variability, with absence of a small L1 regression left inconclusive
(see v3 breakdown below). L10-sequential BEFORE v2 (1645.8) is a +25% baseline outlier vs its
own other samples (1306–1348); AFTER v2 (1354.1) sits inside the baseline's normal band.

Historical compress samples on the intermediate tree (for drift context only): sequential
L2 B {126.8, 137.2, 137.0} / A {146.0, 153.5, 137.7}; zeros L2 B {126.7, 143.2, 143.4} /
A {148.0, 146.1, 144.8}; the reversed-order pair closed the gap then, and the v2 pair on
the final `Option<&dyn>` tree shows L2 within ±0.6% in both groups.

v3 reversed-order pair (AFTER first, then BEFORE; candidate `@` tree — identical source
across the doc-only amends; two in-session
samples each — `--group=` selects but does not exclude in zenbench 0.1.11 iterate mode, so
each `cargo bench` invocation measured the full suite):

| Arm | AFTER v3 (2 samples) | BEFORE v3 (2 samples) |
| --- | --- | --- |
| compress/sequential L1 | 162.8, 137.9 µs | 140.6, 136.9 µs |
| compress/sequential L2 | 138.9, 140.9 µs | 138.6, 137.5 µs |
| compress/sequential L4 | 137.8, 136.9 µs | 140.9, 137.3 µs |
| compress/sequential L6 | 144.7, 153.1 µs | 146.4, 145.3 µs |
| compress/sequential L9 | 157.0, 145.0 µs | 143.7, 148.8 µs |
| compress/sequential L10 | 1401.5, 1525.4 µs | 1439.4, 1527.9 µs |
| compress/sequential L12 | 1518.5, 1557.2 µs | 1786.6, 1593.3 µs |
| parallel L1 1T/2T/4T | 27.8/28.4/28.5; 27.9/28.5/28.6 ms | 27.9/28.6/28.2; 28.0/28.8/28.3 ms |
| parallel L6 1T/2T/4T | 31.9/32.5/33.1; 32.0/32.5/33.0 ms | 32.0/32.6/32.9; 32.1/32.6/32.9 ms |
| parallel L12 1T/2T/4T | 41.1/41.8/42.6; 40.8/41.8/42.5 ms | 41.0/41.9/42.3; 40.8/41.8/42.6 ms |

For Sequential L1, AFTER's own two v3 samples on the identical binary span
`137.9–162.8 µs` (`-15%` intra-session swing), and its low sample (`137.9 µs`) sits
inside BEFORE's `{136.9–140.6}` band (`+0.7%` and `+15.8%` across the two pairs);
across both the superseded intermediate runs and final candidate, pooled ranges overlap
(`{127.1–151.1}` vs `{137.7–162.8}`), demonstrating high variability while leaving
literal zero regression on L1 inconclusive. For Parallel L6 in v3, `1T` (`31.9–32.0` vs
`32.0–32.1 ms`) and `2T` (`32.5` vs `32.6 ms`) are at or below BEFORE while `4T` is
`33.0–33.1` vs `32.9 ms` (`+0.3%` to `+0.6%`), showing the v2 `2T = 35.0 ms` spike did
not reproduce.

Verdict: Parallel L6's v2 `2T` regression does not reproduce in v3 (`1T/2T <= BEFORE`, `4T`
`+0.3%` to `+0.6%`), compress/zeros/sequential L2–L12, decompress, stream_decompress, and
checksums match within noise, and sequential L1 shows high session variance (`+0.7%` /
`+15.8%` in v3) so zero L1 regression is not conclusively proven. Full logs:
`~/tmp/bench-lanes-2026-11-23/zenflate-{BEFORE,AFTER}-bench-v2.log` (final pair),
`zenflate-{BEFORE,AFTER}-seqpar-v3.log` (reversed-order pair), plus
`-bench.log`/`-compress{2,3}.log` (earlier samples); zenbench evidence JSONs under
`.zenbench/data/runs/` in each remote tree.

## Verification

- `cargo check --all-targets`, `--no-default-features`, `--no-default-features --features
  alloc,compress,simd`, wasm32 `alloc,compress,simd` + `scan`: pass.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`: pass.
- `cargo test --release` lib: 294 passed; conformance 10/10 (also
  `ZENFLATE_CONFORMANCE=full`: 10/10, byte-exact matrix); checksum_reuse, fuzz_regression,
  scan: pass. Doctests 16/16. `cargo test --release --features unchecked`: pass.
  Feature gates `--features std` (112) and `--features compress,std,threads` (288): pass.
- `cargo test --release --all-targets` cannot run custom zenbench benches as tests: the
  harness rejects `--test` — identical failure on unmodified baseline (pre-existing).
- `cargo test --release --no-default-features` lib tests fail to compile in pure no_std —
  identical 94 errors on unmodified baseline (pre-existing; the lib itself checks clean).
- Byte parity vs baseline `7270e38f`: a scratch harness (`~/tmp/bench-lanes-2026-11-23/parity`)
  compressed 5 inputs (zeros/seq/rand/text/pngfilt, 0.3–1 MiB) at `new(1..=12)` ×
  {deflate,zlib,gzip} and `png(1,2,3,4,6,9,10,12,15,18,19,22,23,30)` × {deflate,zlib} and
  SHA-256'd every output: **320/320 hashes identical** between the two trees.

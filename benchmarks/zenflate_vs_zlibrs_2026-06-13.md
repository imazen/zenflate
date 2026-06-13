# zenflate vs zlib-rs — DEFLATE throughput & ratio

- Date: 2026-06-13
- Base commit: 5dec719 (bench arm + example added on top)
- Host: lilith (AMD Ryzen 9 7950X, Zen 4), Linux/WSL2
- Build: `--release`, **no** `-C target-cpu=native` (runtime SIMD dispatch only)
- zenflate 0.3.6 vs zlib-rs 0.6.3, **raw DEFLATE** (window_bits -15, no zlib/gzip checksum)
- Speed (synthetic): `cargo bench --bench throughput -- 'zenflate|zlib-rs'` (zenbench/criterion)
- Size + wall-clock (incl. real data): `cargo run --release --example zlib_rs_ratio` (median of 7)

## Headline (compared at MATCHED RATIO, not matched level number)

**Decompress:** zenflate ~13-17% faster than zlib-rs on realistic data, tied on
trivially-compressible input. Decompress speed is ratio-independent, so this is clean.

| decompress (1 MB synthetic) | zenflate | zlib-rs |
|---|---|---|
| sequential | 50.3 us | 51.3 us |
| zeros | 38.0 us | 45.2 us |
| mixed | 1.33 ms | 1.55 ms |
| photo | 1.50 ms | 1.76 ms |

**Compress:** on near-incompressible synthetic data (~1.1x) the two reach the same ratio
and zenflate is ~2x faster at L6-L12 -- but that data is unrepresentative. On real text
(Silesia `dickens`, 10 MB) the level scales diverge: zenflate's ratio plateaus ~2.30-2.36x
through L1-L9 and only reaches ~2.62x at L12; zlib-rs climbs smoothly, reaching 2.62x by L6
and topping out at 2.64x (L9). So "2x faster at the same level number" overstated it --
zenflate's mid levels do less work. Compared at matched ratio:

| dickens, raw deflate | ratio | compress time |
|---|---|---|
| zenflate L12 | 2.62x | 106 ms |
| zlib-rs L6 | 2.62x | 137 ms  -> zenflate ~1.3x faster at equal ratio |
| zlib-rs L9 | 2.64x | 305 ms  -> zenflate ~2.9x faster for ~same ratio |
| zenflate L1 | 2.30x | 35 ms |
| zlib-rs L1 | 1.69x | 33 ms  (zlib-rs L1 compresses much worse) |

Net: zenflate dominates the high-compression end of the speed/ratio frontier (reaches
zlib-rs's best ~2.63x ratio in roughly a third of the time), is ~1.3x faster at matched
mid ratio, and decompresses ~15% faster. zlib-rs's maximum ratio is marginally better
(2.64x vs 2.62x, ~0.8%) at its slowest setting.

## Caveats / fairness

- Raw deflate has no checksum, so zlib-rs's non-default `avx512`/`vpclmulqdq` cargo features
  (which accelerate CRC32/adler for zlib/gzip framing) do not apply here. Match-finder SIMD
  is runtime-dispatched in both, so default-feature zlib-rs is fair for raw-deflate compress.
- Synthetic speed is from criterion (rigorous); real-data (dickens) speed is wall-clock
  median-of-7 in the example (complementary, less rigorous).
- zenflate reuses its `Compressor` across iterations; zlib-rs `compress_slice` allocates
  internal state per call. At >=1 MB this is compute-bound and the per-call alloc is small.
- One machine. Numbers are this hardware; re-run before quoting.

## Raw example output (size + wall-clock)

```

=== mixed (1 MB) — 1000000 bytes ===
level    zf bytes  ratio    zf ms     zl bytes  ratio    zl ms
    1      888888  1.13x    4.90       935164  1.07x    5.68
    2      888888  1.13x    4.97       889341  1.12x   10.15
    4      888888  1.13x    4.90       891460  1.12x   11.30
    6      889415  1.12x    5.80       888432  1.13x   12.13
    9      889415  1.12x    5.78       888900  1.12x   13.81
   10      889498  1.12x    8.17       888900  1.12x   13.84
   12      885573  1.13x    8.12       888900  1.12x   13.85

=== photo (~1 MB RGB) — 998787 bytes ===
level    zf bytes  ratio    zf ms     zl bytes  ratio    zl ms
    1      919306  1.09x    5.30      1054735  0.95x    6.41
    2      919306  1.09x    5.28       918319  1.09x   11.93
    4      919306  1.09x    5.29       918318  1.09x   12.91
    6      919221  1.09x    6.17       918318  1.09x   13.89
    9      919221  1.09x    6.16       913427  1.09x   15.32
   10      920754  1.08x   10.09       913427  1.09x   15.33
   12      919831  1.09x   10.03       913427  1.09x   15.33

=== dickens text (real, ~10 MB) — 10192446 bytes ===
level    zf bytes  ratio    zf ms     zl bytes  ratio    zl ms
    1     4422807  2.30x   35.18      6023619  1.69x   33.05
    2     4422807  2.30x   35.13      4303512  2.37x   57.06
    4     4422807  2.30x   35.16      3958360  2.57x   94.50
    6     4312814  2.36x   40.65      3895405  2.62x  136.66
    9     4312782  2.36x   40.72      3859102  2.64x  307.03
   10     4009652  2.54x   73.16      3859102  2.64x  306.23
   12     3890574  2.62x  106.33      3859102  2.64x  305.36

Compare at MATCHED RATIO (same output size), not matched level number.
```

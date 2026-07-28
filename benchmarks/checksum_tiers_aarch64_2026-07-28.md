# Checksum kernels: per-tier NEON isolation — 2026-07-28

Platform: Apple Silicon (aarch64, NEON), darwin 25.5.0
Bench: `benches/checksum_tiers.rs` (zenbench, interleaved arms)

adler32 and crc32 run over every compressed byte, so they sit on the critical path of every
encode and decode. `throughput.rs` measures whole compression and cannot reveal a checksum
kernel being slower than its own scalar fallback — a real failure mode in this sweep, where
three zenfilters NEON kernels lost to their scalar tier.

## Result: no losers

| kernel | size | NEON | scalar | speedup |
|---|---|---|---|---|
| adler32 | 64 KiB | 2.2 µs | 25.8 µs | **11.7×** |
| adler32 | 4 MiB | 124.3 µs | 1508.0 µs | **12.1×** |
| crc32 | 64 KiB | 3.1 µs | 56.1 µs | **18.1×** |
| crc32 | 4 MiB | 190.3 µs | 3534.5 µs | **18.6×** |

crc32 reaching 20.5 GB/s says the kernel is using the dedicated ARM CRC path rather than a
table-driven loop — that was the specific thing worth checking here, since aarch64 has CRC
instructions and a kernel that ignored them could plausibly lose to the autovectorized scalar
arm. It does not.

adler32 at 31.4 GB/s is a wide horizontal-sum reduction, which is the shape LLVM's
autovectorizer handles poorly and hand-written SIMD handles well — consistent with the other
reduce-shaped kernels in this sweep (zenpixels-convert's predicates at 3.6–4.9×) and unlike
the elementwise passes that correctly measure ~1.00× at the memory-bandwidth wall.

Two sizes are measured because a 64 KiB chunk is a realistic per-block call while 4 MiB shows
steady state once per-call overhead is amortized; the ratios agree, so the win is real
throughput rather than dispatch amortization.

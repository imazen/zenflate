# Checksum tiers on macOS ARM64, 2026-10-16

Source `7b3df5bb`, M4 Pro. Command: `~/work/claudehints/scripts/run-heavy --mem 12G --jobs 4 -- cargo bench --bench checksum_tiers -- --cpu-policy=unrestricted --format=llm`. Full output is `~/tmp/zenflate-checksum-mac.log` on this Mac. The scheduler was unrestricted; the 4 MiB Adler NEON group had 38% CV.

| Input | NEON median | Forced scalar median |
| --- | ---: | ---: |
| Adler-32, 64 KiB | 2.11 µs | 24.97 µs |
| CRC-32, 64 KiB | 2.84 µs | 55.68 µs |
| Adler-32, 4 MiB | 133.92 µs | 1.66 ms |
| CRC-32, 4 MiB | 176.29 µs | 3.50 ms |

The current SIMD paths decisively beat forced scalar on these checksum inputs. These are checksum microbenchmarks, not complete PNG encode or decode measurements. This run supports no checksum dispatch change.

`run-heavy`: peak RSS 0.26 GiB, minimum available memory 5288 MiB. The wrapper reported the full benchmark completed in 41 seconds.

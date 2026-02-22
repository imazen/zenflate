# Upstream Bug Audit

Cross-reference of all known bugs from libdeflate, miniz_oxide, zlib-rs, flate2, and fdeflate
issue trackers, checked against zenflate's codebase. Audit performed 2026-02-21.

## Action Items

- [x] Report bytes consumed when decoding (like libdeflate's `_ex` API). `DecompressOutcome`
  already has both `input_consumed` and `output_written`. Tests now assert on `input_consumed`
  for all three formats. See miniz_oxide #158, libdeflate #420.

## libdeflate

| # | Issue | Status | How verified |
|---|-------|--------|-------------|
| #323 | Huffman codes < 2 codewords | **Safe** | Audited `huffman.rs:345-367`, always ensures >= 2 |
| #102 | Exact compressed size buffer fails | **Tested** | `compress_exact_output_size` — all 13 levels |
| #294 | compress_bound violation | **Tested** | `compress_bound_exact_buffer_all_levels` — 5 patterns x 13 levels x 3 formats |
| #157 | Infinite loop / no forward progress | **Safe** | Audited — overread tracking + bit consumption per iteration |
| #33 | Static table DoS (slow rebuild) | **Safe** | Audited — `static_codes_loaded` flag caches tables |
| #288 | Code length count exceeds expected | **Safe** | Audited — line 903 `i != total_syms` check |
| #403 | SIMD CRC-32 zero-extension | **N/A** | archmage/Rust doesn't have this C footgun |
| #331 | Code lengths crossing litlen/distance | **Safe** | Audited — decoded as single unified sequence |
| #44 | 4GB size overflow | **Safe** | Rust uses `usize` |
| #86 | Level 0 stored blocks | **Tested** | `empty_stored_block_final`, `compress_bound_exact_buffer_all_levels` |
| #106 | Incomplete streams rejected | **Tested** | `sync_flush_nonfinal_block_rejected` |
| #420 | Bytes consumed alignment | **Gap** | We don't report `input_consumed` yet |

## miniz_oxide

| # | Issue | Status | How verified |
|---|-------|--------|-------------|
| #137 | Incomplete Huffman tree accepted | **Tested** | `reject_incomplete_huffman_tree_miniz137` — exact vector |
| #130 | HLIT > 286 not validated | **Intentional** | Matches libdeflate C — both accept 287-288 |
| #161 | Garbage input panic in match copy | **Tested** | `garbage_input_no_panic_miniz161` — exact hex vector |
| #143 | Sync flush / non-final block | **Tested** | `sync_flush_nonfinal_block_rejected` — exact vector |
| #169 | Truncated zlib differs from zlib | **N/A** | Whole-buffer API rejects incomplete streams by design |
| #174 | Short garbage silently accepted | **Tested** | `reject_single_byte_all_formats` + `two_byte_deflate_no_panic` |
| #188 | Stack overflow (large struct on WASM) | **Safe** | Compressor 2.5KB, Decompressor 12.5KB, large buffers Box'd |
| #110 | Exact-size output buffer edge case | **Tested** | `decompress_into_zero_length_output` + compress_bound tests |
| #158 | Over-reported bytes consumed | **Gap** | We don't report `input_consumed` yet |
| #119 | decompress_to_vec fails at exact limit | **N/A** | We don't have a growing-buffer API |

## zlib-rs

| # | Issue | Status | How verified |
|---|-------|--------|-------------|
| #407 | SIMD CRC-32 wrong at 32 bytes | **Covered** | `parity_simd_boundaries` (0-300) + `crc32_all_simd_tiers` permutation |
| #459 | Non-deterministic compression on reuse | **Tested + Audited** | `compression_deterministic_across_reuse` + matchfinder `max_len` clamped to remaining input via `adjust_max_and_nice_len` |
| #455 | Compression Ok on truncated output | **Tested** | `compress_exact_output_size` |
| #232 | SIMD match copy reads uninitialized | **Safe** | `forbid(unsafe_code)`, safe indexing enforces bounds |
| #172 | Gzip fails at certain chunk sizes | **Tested** | `gzip_test_vector_zlibrs172` + streaming chunk stress tests |
| #340 | Stack overflow in inflate | **Safe** | Decompressor 12.5KB, well under 1MB stack |
| #300 | Cross-platform non-determinism | **Safe** | Hash functions are simple arithmetic, not SIMD-dependent |
| #472 | total_out 32-bit overflow | **Safe** | Rust uses `usize` |
| #306 | Intel Raptor Lake CPU bug | **N/A** | Hardware issue |
| #229 | OOB panic after deflateParams | **N/A** | No mid-stream parameter changes |
| #219 | OOB in CRC hash after deflateParams | **N/A** | No mid-stream parameter changes |
| #164 | Invalid stream at Z_BEST_SPEED incremental | **N/A** | Whole-buffer compression API |
| #169 | Z_BEST_SPEED Z_FINISH handling | **N/A** | Whole-buffer compression API |
| #439 | Multi-member gzip trailer | **N/A** | Single-member gzip only |
| #433 | inflate after Z_STREAM_END | **N/A** | Whole-buffer API |

## flate2-rs

| # | Issue | Status | How verified |
|---|-------|--------|-------------|
| #508 | Missing bytes with sync flush | **N/A** | flate2/miniz_oxide streaming integration bug |
| #499 | Short garbage accepted after valid data | **N/A** | flate2 wrapper behavior, not decompressor |
| #474 | Empty input + L0 | **Tested** | `empty_input_level0_roundtrip` |
| #413 | zlib-rs backend panics | **N/A** | zlib-rs maturity issue |
| #392 | Tree borrows UB via C FFI | **N/A** | zenflate is pure Rust |
| #258 | Invalid zlib headers accepted | **Tested** | `reject_invalid_zlib_headers` |

## fdeflate

| # | Issue | Status | How verified |
|---|-------|--------|-------------|
| #25 | Different results at different chunk sizes | **Tested** | `test_stream_chunk_stress_1byte` + `test_stream_chunk_stress_64byte` |
| #40 | Uncompressed block regression | **N/A** | fdeflate-specific state machine bug |
| #48 | EOF symbol bit check | **Safe** | libdeflate handles EOB through Huffman table decode |
| #58 | Returning data with InsufficientInput | **Gap** | Streaming API discards progress on error |

## Test Summary

18 tests added in this audit session (168 → 186 total):

**Structural edge cases:**
- `empty_stored_block_final` — zero-length stored block
- `two_empty_stored_blocks` — chained zero-length stored blocks
- `reject_stored_block_bad_nlen` — LEN/NLEN mismatch
- `reject_reserved_block_type` — block type 3
- `reject_truncated_dynamic_huffman` — too few bytes for header
- `two_byte_deflate_no_panic` — all 65536 two-byte inputs
- `decompress_into_zero_length_output` — empty output buffer

**Checksum/footer validation:**
- `reject_zlib_bad_adler32` — corrupted Adler-32
- `reject_gzip_bad_crc32` — corrupted CRC-32
- `reject_gzip_bad_isize` — corrupted ISIZE

**Upstream-specific test vectors:**
- `reject_incomplete_huffman_tree_miniz137` — miniz_oxide #137 vector
- `garbage_input_no_panic_miniz161` — miniz_oxide #161 hex vector
- `sync_flush_nonfinal_block_rejected` — miniz_oxide #143 vector
- `gzip_test_vector_zlibrs172` — zlib-rs #172 "expected output"

**Compression robustness:**
- `compress_bound_exact_buffer_all_levels` — 5 patterns x 13 levels x 3 formats
- `compress_exact_output_size` — exact compressed size buffer at all levels
- `compression_deterministic_across_reuse` — reuse doesn't change output
- `decompressor_reuse_across_formats` — stale state regression

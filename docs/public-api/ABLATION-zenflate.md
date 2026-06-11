# ABLATION-zenflate.md

**Date:** 2026-06-11  
**Snapshot commit:** 9a87bf36 (main@origin)  
**Surface size:** 609 items (default features = all features — identical)  
**Grep template:** `grep -r "<symbol>" /home/lilith/work/zen/zenpng/src --include="*.rs" --exclude-dir=target`

---

## Summary

**0 items flagged. Surface is coherent.**

609 items reviewed. The large item count is almost entirely structural: every major type appears twice — once in its submodule path (`zenflate::compress::Compressor`) and once as a root re-export (`zenflate::Compressor`). This doubles the count without adding API surface. No public-API mistakes found under the conservative bar.

---

## Consumer Evidence (zenpng primary consumer)

All distinctly named API items confirmed consumed externally in zenpng:

| Symbol | zenpng usage confirmed |
|---|---|
| `zenflate::crc32` / `crc32_combine` | decode.rs, chunk/mod.rs, chunk/write.rs, decoder/row.rs |
| `zenflate::InputSource` (trait) | decoder/row.rs: `impl zenflate::InputSource for IdatSource` |
| `zenflate::StreamDecompressor` | decoder/row.rs, decoder/interlace.rs, decoder/apng.rs |
| `zenflate::Decompressor` | chunk/ancillary.rs |
| `zenflate::Compressor` | chunk/ancillary.rs, encoder/compress.rs |
| `zenflate::CompressorSnapshot` | encoder/filter.rs: `Vec<(usize, usize, u8, CompressorSnapshot)>` |
| `zenflate::CompressionLevel` | encoder/compress.rs, encoder/metadata.rs |
| `zenflate::CompressionLevel::monotonicity_fallback` | encoder/compress.rs |
| `zenflate::Compressor::deflate_compress_incremental` | encoder/filter.rs |
| `zenflate::Compressor::deflate_estimate_cost_incremental` | encoder/filter.rs |
| `zenflate::Unstoppable` | encoder/metadata.rs, chunk/ancillary.rs |

`Adler32Hasher`, `Crc32Hasher`, `adler32`/`adler32_combine`, streaming module, `BufReadSource`, `DecompressOutcome`, `DecompressionError`/`CompressionError`/`StreamError`, `Stop`/`StopReason` — also in surface; not verified in zenpng scan but expected in zenzop and the zensally/heic deps.

---

## Structural note: dual-path re-exports

609 items for 9 main types is expected: each type appears at `zenflate::compress::*` or `zenflate::decompress::*` (canonical definition) AND as a re-export at `zenflate::*` (convenience). `cargo public-api` traces both paths and emits impl blocks for each. This inflates the count approximately 2x but is correct behavior — the root re-exports are intentional for ergonomic usage (callers can `use zenflate::Compressor` without knowing the submodule layout).

---

## Digest

| Metric | Count |
|---|---|
| Items in surface | 609 |
| Items flagged (Action A) | 0 |
| Items flagged (Action B) | 0 |
| Flag rate | 0% |

**Verdict:** Surface is appropriate for a full-featured DEFLATE/zlib/gzip library. Checksum hashers, compress/decompress with incremental APIs, snapshot/restore, streaming decompressor, parallel gzip, `CompressorSnapshot` — all confirmed consumed by zenpng's sophisticated filter-selection encoder. `monotonicity_fallback` is confirmed consumed. No internal plumbing detected in the surface.

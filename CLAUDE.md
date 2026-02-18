# zenflate

Pure Rust port of libdeflate. DEFLATE/zlib/gzip compression and decompression.

## Architecture
- Port of libdeflate (~14,500 lines C) to safe Rust with `#![forbid(unsafe_code)]`
- SIMD via archmage/magetypes
- Side-by-side testing against C via `libdeflater` crate

## Source Reference
- C source: `/home/lilith/work/libdeflate-src/lib/`

## Module Map
- `src/constants.rs` — DEFLATE format constants (from deflate_constants.h)
- `src/error.rs` — Error types
- `src/checksum/` — Adler-32 and CRC-32 (scalar + SIMD)
- `src/decompress/` — Decompression (bitstream reader, Huffman tables, inflate loop)
- `src/compress/` — Compression (bitstream writer, Huffman construction, block flushing, strategies)
- `src/matchfinder/` — Hash table, hash chain, binary tree matchfinders
- `src/gzip.rs` — gzip wrapper
- `src/zlib.rs` — zlib wrapper

## Implementation Status
- [ ] Phase 1: Foundation + Checksums
- [ ] Phase 2: Decompression
- [ ] Phase 3: Compression Core
- [ ] Phase 4: Compression Strategies
- [ ] Phase 5: SIMD Acceleration
- [ ] Phase 6: Benchmarks + Polish

## Known Bugs
(none yet)

# Changelog

## Unreleased

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
  - `optimize_huffman_for_rle` functions
  - PNG-specific cost model biases
- `CompressorSnapshot` and cost estimation for incremental API
- `#[must_use]`, `#[non_exhaustive]`, and `Debug` impls on public types
- README badges, MSRV section, and AI disclosure

### Fixed
- Lazy2 off-by-one in incremental compression skip count
- Hash update guard against OOB in greedy match skip loops
- Swap (dist, length) -> (length, dist) return order from match loop
- `fuse_7` precode encoding counted 8 positions instead of 7
- ECT optimizations suppressed in libdeflate compat mode

### Changed
- Removed PNG cost bias from core zenflate (moved to codec layer)
- Reuse `HuffmanScratch` in block splitting, use `FnMut`
- Edition 2024, MSRV 1.89
- Bumped `safe_unaligned_simd` minimum to 0.2.5
- Updated archmage/magetypes to 0.9

## 0.2.1

Initial published release.

## 0.2.0

Initial release. Pure Rust DEFLATE/zlib/gzip compression and decompression,
ported from libdeflate with safe Rust and archmage SIMD.

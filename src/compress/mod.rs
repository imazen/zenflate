//! DEFLATE/zlib/gzip compression.
//!
//! Ported from libdeflate's `deflate_compress.c`, `zlib_compress.c`, `gzip_compress.c`.

pub(crate) mod bitstream;
pub(crate) mod block;
pub(crate) mod block_split;
pub(crate) mod huffman;
pub(crate) mod near_optimal;
pub(crate) mod sequences;

use crate::checksum::{adler32, crc32};
use crate::constants::*;
use crate::error::CompressionError;
use crate::matchfinder::MATCHFINDER_WINDOW_SIZE;
use crate::matchfinder::bt::{BT_MATCHFINDER_REQUIRED_NBYTES, LzMatch};
use crate::matchfinder::hc::HcMatchfinder;
use crate::matchfinder::ht::{HT_MATCHFINDER_REQUIRED_NBYTES, HtMatchfinder};
use crate::matchfinder::lz_hash;

use self::bitstream::OutputBitstream;
use self::block::{DeflateCodes, DeflateFreqs, choose_literal, choose_match, finish_block};
use self::block_split::{BlockSplitStats, MIN_BLOCK_LENGTH};
use self::near_optimal::{
    MATCH_CACHE_LENGTH, NearOptimalState, clear_old_stats, init_stats, merge_stats,
    optimize_and_flush_block, save_stats,
};
use self::sequences::Sequence;

/// Hash order for the ht_matchfinder (needed for initial hash computation).
const HT_MATCHFINDER_HASH_ORDER: u32 = 15;

/// Soft maximum block length (uncompressed bytes). Blocks are ended around here.
const SOFT_MAX_BLOCK_LENGTH: usize = 300000;

/// Maximum number of sequences for greedy/lazy/lazy2 strategies.
const SEQ_STORE_LENGTH: usize = 50000;

/// Soft maximum block length for the fastest strategy.
const FAST_SOFT_MAX_BLOCK_LENGTH: usize = 65535;

/// Maximum number of sequences for the fastest strategy.
const FAST_SEQ_STORE_LENGTH: usize = 8192;

/// Compression level (0-12).
///
/// Higher levels produce smaller output but take longer. Level 6 is a good default.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// let level = CompressionLevel::DEFAULT; // level 6
/// assert_eq!(level.level(), 6);
///
/// // Out-of-range values are clamped
/// assert_eq!(CompressionLevel::new(99).level(), 12);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressionLevel(u32);

impl CompressionLevel {
    /// Create a compression level. Clamps to 0-12.
    pub fn new(level: u32) -> Self {
        Self(level.min(12))
    }

    /// Get the numeric level.
    pub fn level(self) -> u32 {
        self.0
    }

    /// Level 0: no compression (uncompressed blocks only).
    pub const NONE: Self = Self(0);
    /// Level 1: fastest compression.
    pub const FASTEST: Self = Self(1);
    /// Level 6: default compression (good balance of speed and ratio).
    pub const DEFAULT: Self = Self(6);
    /// Level 9: maximum compression with greedy/lazy strategies.
    pub const BEST_GREEDY: Self = Self(9);
    /// Level 12: maximum compression with near-optimal parsing.
    pub const BEST: Self = Self(12);
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// DEFLATE/zlib/gzip compressor.
///
/// Reuse across multiple compressions for best performance (avoids re-initialization).
///
/// ```
/// use zenflate::{Compressor, CompressionLevel};
///
/// let mut compressor = Compressor::new(CompressionLevel::DEFAULT);
///
/// let data = b"Hello, World! Hello, World! Hello, World!";
/// let bound = Compressor::deflate_compress_bound(data.len());
/// let mut out = vec![0u8; bound];
/// let size = compressor.deflate_compress(data, &mut out).unwrap();
/// assert!(size < data.len()); // compressed
/// ```
pub struct Compressor {
    /// Compression level.
    level: CompressionLevel,
    /// Maximum search depth for matchfinding.
    max_search_depth: u32,
    /// "Nice" match length: stop searching if we find a match this long.
    nice_match_length: u32,
    /// Inputs shorter than this are passed through as uncompressed blocks.
    max_passthrough_size: usize,
    /// Current block's frequency counters.
    freqs: DeflateFreqs,
    /// Block split statistics.
    split_stats: BlockSplitStats,
    /// Dynamic Huffman codes for the current block.
    codes: DeflateCodes,
    /// Static Huffman codes.
    static_codes: DeflateCodes,
    /// Sequence store for greedy/lazy/lazy2/fastest strategies.
    sequences: Vec<Sequence>,
    /// Hash table matchfinder for level 1.
    ht_mf: Option<Box<HtMatchfinder>>,
    /// Hash chains matchfinder for levels 2-9.
    hc_mf: Option<Box<HcMatchfinder>>,
    /// Near-optimal state for levels 10-12.
    near_optimal: Option<Box<NearOptimalState>>,
}

impl Compressor {
    /// Create a new compressor at the given compression level.
    #[cfg(feature = "alloc")]
    pub fn new(level: CompressionLevel) -> Self {
        let lvl = level.level();

        let (max_search_depth, nice_match_length) = match lvl {
            0 => (0, 0),
            1 => (0, 32), // ht_matchfinder has hardcoded depth
            2 => (6, 10),
            3 => (12, 14),
            4 => (16, 30),
            5 => (16, 30),
            6 => (35, 65),
            7 => (100, 130),
            8 => (300, DEFLATE_MAX_MATCH_LEN),
            9 => (600, DEFLATE_MAX_MATCH_LEN),
            10 => (35, 75),
            11 => (100, 150),
            12 => (300, DEFLATE_MAX_MATCH_LEN),
            _ => unreachable!(),
        };

        let max_passthrough_size = if lvl == 0 {
            usize::MAX
        } else {
            55 - (lvl as usize * 4)
        };

        let seq_capacity = if lvl == 1 {
            FAST_SEQ_STORE_LENGTH + 1
        } else if (2..=9).contains(&lvl) {
            SEQ_STORE_LENGTH + 1
        } else {
            0
        };

        let mut freqs = DeflateFreqs::default();
        let mut static_codes = DeflateCodes::default();
        block::init_static_codes(&mut freqs, &mut static_codes);
        freqs.reset();

        Self {
            level,
            max_search_depth,
            nice_match_length,
            max_passthrough_size,
            freqs,
            split_stats: BlockSplitStats::new(),
            codes: DeflateCodes::default(),
            static_codes,
            sequences: alloc::vec![Sequence::default(); seq_capacity],
            ht_mf: if lvl == 1 {
                Some(Box::new(HtMatchfinder::new()))
            } else {
                None
            },
            hc_mf: if (2..=9).contains(&lvl) {
                Some(Box::new(HcMatchfinder::new()))
            } else {
                None
            },
            near_optimal: if lvl >= 10 {
                let (passes, improvement, nonfinal, static_opt) = match lvl {
                    10 => (2, 32, 32, 0),
                    11 => (4, 16, 16, 1000),
                    _ => (10, 1, 1, 10000),
                };
                Some(Box::new(NearOptimalState::new(
                    passes,
                    improvement,
                    nonfinal,
                    static_opt,
                )))
            } else {
                None
            },
        }
    }

    /// Compress data in raw DEFLATE format.
    pub fn deflate_compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CompressionError> {
        if input.len() <= self.max_passthrough_size {
            return deflate_compress_none(input, output);
        }

        let mut os = OutputBitstream::new(output);

        match self.level.level() {
            0 => {
                return deflate_compress_none(input, output);
            }
            1 => {
                self.compress_fastest(&mut os, input);
            }
            2..=4 => {
                self.compress_greedy(&mut os, input);
            }
            5..=7 => {
                self.compress_lazy_generic(&mut os, input, false);
            }
            8..=9 => {
                self.compress_lazy_generic(&mut os, input, true);
            }
            10..=12 => {
                self.compress_near_optimal(&mut os, input);
            }
            _ => {
                self.compress_literals(&mut os, input);
            }
        }

        if os.overflow {
            return Err(CompressionError::InsufficientSpace);
        }

        // Write final partial byte if needed
        if os.bitcount > 0 {
            if os.pos < os.buf.len() {
                os.buf[os.pos] = os.bitbuf as u8;
                os.pos += 1;
            } else {
                return Err(CompressionError::InsufficientSpace);
            }
        }

        Ok(os.pos)
    }

    /// Compress data in zlib format (2-byte header + DEFLATE + Adler-32).
    pub fn zlib_compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CompressionError> {
        // zlib header: CMF=0x78, FLG depends on level
        let flg = match self.level.level() {
            0..=1 => 0x01u8,  // fastest
            2..=5 => 0x5Eu8,  // fast
            6 => 0x9Cu8,      // default
            7..=12 => 0xDAu8, // best
            _ => 0x9Cu8,
        };
        // CMF = 0x78 (deflate, window size 32K)
        let cmf = 0x78u8;
        // Adjust FLG so (CMF*256 + FLG) % 31 == 0
        let check = ((cmf as u16) * 256 + flg as u16) % 31;
        let flg = if check == 0 {
            flg
        } else {
            flg + (31 - check) as u8
        };

        if output.len() < 6 {
            return Err(CompressionError::InsufficientSpace);
        }
        output[0] = cmf;
        output[1] = flg;

        let compressed_size = self.deflate_compress(input, &mut output[2..])?;
        let total = 2 + compressed_size;

        // Adler-32 checksum
        let checksum = adler32(1, input);
        if total + 4 > output.len() {
            return Err(CompressionError::InsufficientSpace);
        }
        output[total..total + 4].copy_from_slice(&checksum.to_be_bytes());
        Ok(total + 4)
    }

    /// Compress data in gzip format (10-byte header + DEFLATE + CRC-32 + ISIZE).
    pub fn gzip_compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CompressionError> {
        if output.len() < 18 {
            return Err(CompressionError::InsufficientSpace);
        }
        // gzip header (10 bytes)
        output[0] = 0x1F; // ID1
        output[1] = 0x8B; // ID2
        output[2] = 0x08; // CM = deflate
        output[3] = 0x00; // FLG = none
        output[4..8].copy_from_slice(&[0, 0, 0, 0]); // MTIME
        output[8] = 0x00; // XFL
        output[9] = 0xFF; // OS = unknown

        let compressed_size = self.deflate_compress(input, &mut output[10..])?;
        let total = 10 + compressed_size;

        // CRC-32 + ISIZE (8 bytes)
        if total + 8 > output.len() {
            return Err(CompressionError::InsufficientSpace);
        }
        let checksum = crc32(0, input);
        output[total..total + 4].copy_from_slice(&checksum.to_le_bytes());
        let isize = (input.len() as u32).to_le_bytes();
        output[total + 4..total + 8].copy_from_slice(&isize);
        Ok(total + 8)
    }

    /// Compute the maximum compressed size for raw DEFLATE output.
    pub fn deflate_compress_bound(input_len: usize) -> usize {
        let max_blocks = (input_len + MIN_BLOCK_LENGTH - 1)
            .checked_div(MIN_BLOCK_LENGTH)
            .unwrap_or(0)
            .max(1);
        5 * max_blocks + input_len
    }

    /// Compute the maximum compressed size for zlib output.
    pub fn zlib_compress_bound(input_len: usize) -> usize {
        Self::deflate_compress_bound(input_len) + 2 + 4 // header + adler32
    }

    /// Compute the maximum compressed size for gzip output.
    pub fn gzip_compress_bound(input_len: usize) -> usize {
        Self::deflate_compress_bound(input_len) + 10 + 8 // header + crc32 + isize
    }

    /// Level 1: fastest compression using hash table matchfinder.
    ///
    /// Simple greedy: find longest match, take it or emit literal.
    /// No block splitting (uses fixed FAST_SOFT_MAX_BLOCK_LENGTH).
    fn compress_fastest(&mut self, os: &mut OutputBitstream<'_>, input: &[u8]) {
        let mut mf = self.ht_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = 0usize;
        let mut in_base_offset = 0usize;

        while in_next < in_end && !os.overflow {
            let in_block_begin = in_next;
            let in_max_block_end =
                choose_max_block_end(in_next, in_end, FAST_SOFT_MAX_BLOCK_LENGTH);
            let mut seq_idx = 0;

            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            // Precompute first hash for this block
            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(
                    crate::fast_bytes::load_u32_le(input, in_next),
                    HT_MATCHFINDER_HASH_ORDER,
                )
            } else {
                0
            };

            while in_next < in_max_block_end && seq_idx < FAST_SEQ_STORE_LENGTH {
                let remaining = in_end - in_next;
                let max_len = remaining.min(DEFLATE_MAX_MATCH_LEN as usize) as u32;
                let nice_len = max_len.min(self.nice_match_length);

                if max_len >= HT_MATCHFINDER_REQUIRED_NBYTES {
                    let (length, offset) = mf.longest_match(
                        input,
                        &mut in_base_offset,
                        in_next,
                        max_len,
                        nice_len,
                        &mut next_hash,
                    );

                    if length > 0 {
                        seq_idx = choose_match(
                            &mut self.freqs,
                            length,
                            offset,
                            &mut self.sequences,
                            seq_idx,
                        );
                        if length > 1 {
                            mf.skip_bytes(
                                input,
                                &mut in_base_offset,
                                in_next + 1,
                                length - 1,
                                &mut next_hash,
                            );
                        }
                        in_next += length as usize;
                        continue;
                    }
                }

                choose_literal(
                    &mut self.freqs,
                    input[in_next],
                    &mut self.sequences[seq_idx],
                );
                in_next += 1;
            }

            let block_length = in_next - in_block_begin;
            let is_final = in_next >= in_end;
            finish_block(
                os,
                &input[in_block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final,
            );
        }

        self.ht_mf = Some(mf);
    }

    /// Levels 2-4: greedy compression using hash chains matchfinder.
    ///
    /// Always takes the longest match at each position. Uses block splitting
    /// and adaptive min_match_len heuristic.
    fn compress_greedy(&mut self, os: &mut OutputBitstream<'_>, input: &[u8]) {
        let mut mf = self.hc_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = 0usize;
        let mut in_base_offset = 0usize;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut next_hashes = [0u32; 2];
        let max_search_depth = self.max_search_depth;

        while in_next < in_end && !os.overflow {
            let in_block_begin = in_next;
            let in_max_block_end = choose_max_block_end(in_next, in_end, SOFT_MAX_BLOCK_LENGTH);
            let mut seq_idx = 0;

            self.split_stats = BlockSplitStats::new();
            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let min_len =
                calculate_min_match_len(&input[in_next..in_max_block_end], max_search_depth);

            loop {
                adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);

                let (length, offset) = mf.longest_match(
                    input,
                    &mut in_base_offset,
                    in_next,
                    min_len - 1,
                    max_len,
                    nice_len,
                    max_search_depth,
                    &mut next_hashes,
                );

                if length >= min_len && (length > DEFLATE_MIN_MATCH_LEN || offset <= 4096) {
                    seq_idx = choose_match(
                        &mut self.freqs,
                        length,
                        offset,
                        &mut self.sequences,
                        seq_idx,
                    );
                    self.split_stats.observe_match(length);
                    mf.skip_bytes(
                        input,
                        &mut in_base_offset,
                        in_next + 1,
                        in_end,
                        length - 1,
                        &mut next_hashes,
                    );
                    in_next += length as usize;
                } else {
                    choose_literal(
                        &mut self.freqs,
                        input[in_next],
                        &mut self.sequences[seq_idx],
                    );
                    self.split_stats.observe_literal(input[in_next]);
                    in_next += 1;
                }

                if in_next >= in_max_block_end
                    || seq_idx >= SEQ_STORE_LENGTH
                    || self
                        .split_stats
                        .should_end_block(in_block_begin, in_next, in_end)
                {
                    break;
                }
            }

            let block_length = in_next - in_block_begin;
            let is_final = in_next >= in_end;
            finish_block(
                os,
                &input[in_block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final,
            );
        }

        self.hc_mf = Some(mf);
    }

    /// Levels 5-9: lazy/lazy2 compression using hash chains matchfinder.
    ///
    /// Before committing to a match, looks ahead 1 position (lazy) or 2
    /// positions (lazy2) for a better match. Uses block splitting and
    /// adaptive min_match_len with periodic recalculation.
    fn compress_lazy_generic(&mut self, os: &mut OutputBitstream<'_>, input: &[u8], lazy2: bool) {
        let mut mf = self.hc_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = 0usize;
        let mut in_base_offset = 0usize;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut next_hashes = [0u32; 2];
        let max_search_depth = self.max_search_depth;

        while in_next < in_end && !os.overflow {
            let in_block_begin = in_next;
            let in_max_block_end = choose_max_block_end(in_next, in_end, SOFT_MAX_BLOCK_LENGTH);
            let mut seq_idx = 0;
            let mut next_recalc_min_len = in_next + (in_end - in_next).min(10000);

            self.split_stats = BlockSplitStats::new();
            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let mut min_len =
                calculate_min_match_len(&input[in_next..in_max_block_end], max_search_depth);

            loop {
                // Recalculate min_len periodically based on actual frequency distribution
                if in_next >= next_recalc_min_len {
                    min_len = recalculate_min_match_len(&self.freqs, max_search_depth);
                    next_recalc_min_len +=
                        (in_end - next_recalc_min_len).min(in_next - in_block_begin);
                }

                // Find match at current position
                adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                let (mut cur_len, mut cur_offset) = mf.longest_match(
                    input,
                    &mut in_base_offset,
                    in_next,
                    min_len - 1,
                    max_len,
                    nice_len,
                    max_search_depth,
                    &mut next_hashes,
                );

                if cur_len < min_len || (cur_len == DEFLATE_MIN_MATCH_LEN && cur_offset > 8192) {
                    // No usable match — emit literal
                    choose_literal(
                        &mut self.freqs,
                        input[in_next],
                        &mut self.sequences[seq_idx],
                    );
                    self.split_stats.observe_literal(input[in_next]);
                    in_next += 1;
                } else {
                    // Have a match. Advance past the match start position.
                    in_next += 1;

                    // Lazy evaluation loop (simulates C goto have_cur_match)
                    // Invariant: match at (in_next - 1), length cur_len, offset cur_offset
                    loop {
                        if cur_len >= nice_len {
                            // Very long match — take it immediately
                            seq_idx = choose_match(
                                &mut self.freqs,
                                cur_len,
                                cur_offset,
                                &mut self.sequences,
                                seq_idx,
                            );
                            self.split_stats.observe_match(cur_len);
                            mf.skip_bytes(
                                input,
                                &mut in_base_offset,
                                in_next,
                                in_end,
                                cur_len - 1,
                                &mut next_hashes,
                            );
                            in_next += (cur_len - 1) as usize;
                            break;
                        }

                        // Look ahead: try to find a better match at the next position.
                        // Use half the search depth for the lookahead.
                        adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                        let (next_len, next_offset) = mf.longest_match(
                            input,
                            &mut in_base_offset,
                            in_next,
                            cur_len - 1,
                            max_len,
                            nice_len,
                            max_search_depth >> 1,
                            &mut next_hashes,
                        );
                        in_next += 1;

                        if next_len >= cur_len
                            && 4 * (next_len as i32 - cur_len as i32)
                                + (bsr32(cur_offset) as i32 - bsr32(next_offset) as i32)
                                > 2
                        {
                            // Better match at next position — emit literal, adopt new match
                            choose_literal(
                                &mut self.freqs,
                                input[in_next - 2],
                                &mut self.sequences[seq_idx],
                            );
                            self.split_stats.observe_literal(input[in_next - 2]);
                            cur_len = next_len;
                            cur_offset = next_offset;
                            continue; // back to have_cur_match
                        }

                        if lazy2 {
                            // Second lookahead with quarter search depth
                            adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                            let (next_len2, next_offset2) = mf.longest_match(
                                input,
                                &mut in_base_offset,
                                in_next,
                                cur_len - 1,
                                max_len,
                                nice_len,
                                max_search_depth >> 2,
                                &mut next_hashes,
                            );
                            in_next += 1;

                            if next_len2 >= cur_len
                                && 4 * (next_len2 as i32 - cur_len as i32)
                                    + (bsr32(cur_offset) as i32 - bsr32(next_offset2) as i32)
                                    > 6
                            {
                                // Much better match 2 ahead — emit 2 literals
                                choose_literal(
                                    &mut self.freqs,
                                    input[in_next - 3],
                                    &mut self.sequences[seq_idx],
                                );
                                self.split_stats.observe_literal(input[in_next - 3]);
                                choose_literal(
                                    &mut self.freqs,
                                    input[in_next - 2],
                                    &mut self.sequences[seq_idx],
                                );
                                self.split_stats.observe_literal(input[in_next - 2]);
                                cur_len = next_len2;
                                cur_offset = next_offset2;
                                continue; // back to have_cur_match
                            }

                            // No better match — take the original
                            seq_idx = choose_match(
                                &mut self.freqs,
                                cur_len,
                                cur_offset,
                                &mut self.sequences,
                                seq_idx,
                            );
                            self.split_stats.observe_match(cur_len);
                            if cur_len > 3 {
                                mf.skip_bytes(
                                    input,
                                    &mut in_base_offset,
                                    in_next,
                                    in_end,
                                    cur_len - 3,
                                    &mut next_hashes,
                                );
                                in_next += (cur_len - 3) as usize;
                            }
                        } else {
                            // No better match — take the original (lazy, not lazy2)
                            seq_idx = choose_match(
                                &mut self.freqs,
                                cur_len,
                                cur_offset,
                                &mut self.sequences,
                                seq_idx,
                            );
                            self.split_stats.observe_match(cur_len);
                            mf.skip_bytes(
                                input,
                                &mut in_base_offset,
                                in_next,
                                in_end,
                                cur_len - 2,
                                &mut next_hashes,
                            );
                            in_next += (cur_len - 2) as usize;
                        }
                        break;
                    }
                }

                // Check if block should end
                if in_next >= in_max_block_end
                    || seq_idx >= SEQ_STORE_LENGTH
                    || self
                        .split_stats
                        .should_end_block(in_block_begin, in_next, in_end)
                {
                    break;
                }
            }

            let block_length = in_next - in_block_begin;
            let is_final = in_next >= in_end;
            finish_block(
                os,
                &input[in_block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final,
            );
        }

        self.hc_mf = Some(mf);
    }

    /// Levels 10-12: near-optimal compression using binary tree matchfinder.
    ///
    /// Finds all matches at each position, caches them, then uses iterative
    /// backward DP to find the minimum-cost literal/match path.
    fn compress_near_optimal(&mut self, os: &mut OutputBitstream<'_>, input: &[u8]) {
        let mut ns = self.near_optimal.take().unwrap();
        ns.bt_mf.init();

        let in_end = input.len();
        let mut in_next = 0usize;
        let mut in_base_offset = 0usize;
        let mut in_next_slide = in_next + (in_end - in_next).min(MATCHFINDER_WINDOW_SIZE as usize);
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut cache_idx = 0usize;
        let mut next_hashes = [0u32; 2];
        let mut prev_block_used_only_literals = false;
        let max_search_depth = self.max_search_depth;
        let mut in_block_begin = 0usize;

        init_stats(&mut self.split_stats, &mut ns);

        loop {
            // Starting a new DEFLATE block
            let in_max_block_end =
                choose_max_block_end(in_block_begin, in_end, SOFT_MAX_BLOCK_LENGTH);
            let mut prev_end_block_check: Option<usize> = None;
            let mut change_detected = false;
            let mut next_observation = in_next;

            // Use min_match_len heuristic for observation statistics only.
            // The actual DP parse considers all match lengths.
            let min_len = if prev_block_used_only_literals {
                DEFLATE_MAX_MATCH_LEN + 1
            } else {
                calculate_min_match_len(&input[in_block_begin..in_max_block_end], max_search_depth)
            };

            // Find matches until we decide to end the block
            loop {
                let remaining = in_end - in_next;

                // Slide the window forward if needed
                if in_next == in_next_slide {
                    ns.bt_mf.slide_window();
                    in_base_offset = in_next;
                    in_next_slide = in_next + remaining.min(MATCHFINDER_WINDOW_SIZE as usize);
                }

                // Find matches at current position
                let matches_start = cache_idx;
                let mut best_len = 0u32;
                adjust_max_and_nice_len(&mut max_len, &mut nice_len, remaining);

                if max_len >= BT_MATCHFINDER_REQUIRED_NBYTES {
                    let cur_pos = (in_next as isize - in_base_offset as isize) as i32;
                    let num_matches = ns.bt_mf.get_matches(
                        input,
                        in_base_offset,
                        cur_pos,
                        max_len,
                        nice_len,
                        max_search_depth,
                        &mut next_hashes,
                        &mut ns.match_cache[cache_idx..],
                    );
                    cache_idx += num_matches;
                    if num_matches > 0 {
                        best_len = ns.match_cache[cache_idx - 1].length as u32;
                    }
                }

                // Track observations for block splitting
                if in_next >= next_observation {
                    if best_len >= min_len {
                        self.split_stats.observe_match(best_len);
                        next_observation = in_next + best_len as usize;
                        ns.new_match_len_freqs[best_len as usize] += 1;
                    } else {
                        self.split_stats.observe_literal(input[in_next]);
                        next_observation = in_next + 1;
                    }
                }

                // Write sentinel: num_matches and literal value
                let num_matches = cache_idx - matches_start;
                ns.match_cache[cache_idx] = LzMatch {
                    length: num_matches as u16,
                    offset: input[in_next] as u16,
                };
                in_next += 1;
                cache_idx += 1;

                // Skip bytes covered by a nice-length match.
                // Avoids degenerate behavior on highly redundant data.
                if best_len >= DEFLATE_MIN_MATCH_LEN && best_len >= nice_len {
                    let mut skip = best_len - 1;
                    while skip > 0 {
                        let remaining = in_end - in_next;
                        if in_next == in_next_slide {
                            ns.bt_mf.slide_window();
                            in_base_offset = in_next;
                            in_next_slide =
                                in_next + remaining.min(MATCHFINDER_WINDOW_SIZE as usize);
                        }
                        adjust_max_and_nice_len(&mut max_len, &mut nice_len, remaining);
                        if max_len >= BT_MATCHFINDER_REQUIRED_NBYTES {
                            let cur_pos = (in_next as isize - in_base_offset as isize) as i32;
                            ns.bt_mf.skip_byte(
                                input,
                                in_base_offset,
                                cur_pos,
                                nice_len,
                                max_search_depth,
                                &mut next_hashes,
                            );
                        }
                        // Sentinel for skipped position (no matches)
                        ns.match_cache[cache_idx] = LzMatch {
                            length: 0,
                            offset: input[in_next] as u16,
                        };
                        in_next += 1;
                        cache_idx += 1;
                        skip -= 1;
                    }
                }

                // Maximum block length or end of input reached?
                if in_next >= in_max_block_end {
                    break;
                }
                // Match cache overflowed?
                if cache_idx >= MATCH_CACHE_LENGTH {
                    break;
                }
                // Not ready to check block end?
                if !self
                    .split_stats
                    .ready_to_check(in_block_begin, in_next, in_end)
                {
                    continue;
                }
                // Check if it would be worthwhile to end the block
                if self
                    .split_stats
                    .do_end_block_check((in_next - in_block_begin) as u32)
                {
                    change_detected = true;
                    break;
                }
                // Not ending — merge stats and record checkpoint
                merge_stats(&mut self.split_stats, &mut ns);
                prev_end_block_check = Some(in_next);
            }

            // All matches for this block have been cached. Flush.
            if let (true, Some(in_block_end)) = (change_detected, prev_end_block_check) {
                // Rewind to just before the differing chunk.
                let block_length = (in_block_end - in_block_begin) as u32;
                let is_first = in_block_begin == 0;
                let num_bytes_to_rewind = in_next - in_block_end;

                // Rewind the match cache
                let orig_cache_idx = cache_idx;
                let mut rewind_count = num_bytes_to_rewind;
                while rewind_count > 0 {
                    cache_idx -= 1; // sentinel
                    cache_idx -= ns.match_cache[cache_idx].length as usize;
                    rewind_count -= 1;
                }
                let cache_len_rewound = orig_cache_idx - cache_idx;

                prev_block_used_only_literals = optimize_and_flush_block(
                    &mut ns,
                    os,
                    &input[in_block_begin..],
                    block_length,
                    cache_idx,
                    is_first,
                    false,
                    &mut self.freqs,
                    &mut self.codes,
                    &self.static_codes,
                    &self.split_stats,
                    max_search_depth,
                );

                // Move remaining cache entries to beginning
                ns.match_cache
                    .copy_within(cache_idx..cache_idx + cache_len_rewound, 0);
                cache_idx = cache_len_rewound;

                save_stats(&self.split_stats, &mut ns);
                clear_old_stats(&mut self.split_stats, &mut ns);
                in_block_begin = in_block_end;
            } else {
                // End block at current position (no rewind)
                let block_length = (in_next - in_block_begin) as u32;
                let is_first = in_block_begin == 0;
                let is_final = in_next == in_end;

                merge_stats(&mut self.split_stats, &mut ns);
                prev_block_used_only_literals = optimize_and_flush_block(
                    &mut ns,
                    os,
                    &input[in_block_begin..],
                    block_length,
                    cache_idx,
                    is_first,
                    is_final,
                    &mut self.freqs,
                    &mut self.codes,
                    &self.static_codes,
                    &self.split_stats,
                    max_search_depth,
                );

                cache_idx = 0;
                save_stats(&self.split_stats, &mut ns);
                init_stats(&mut self.split_stats, &mut ns);
                in_block_begin = in_next;
            }

            if in_next >= in_end || os.overflow {
                break;
            }
        }

        self.near_optimal = Some(ns);
    }

    /// Simple literal-only compressor that exercises the full block flushing path.
    fn compress_literals(&mut self, os: &mut OutputBitstream<'_>, input: &[u8]) {
        let mut pos = 0;

        while pos < input.len() && !os.overflow {
            // Start a new block
            let block_begin = pos;
            let max_block_end = choose_max_block_end(pos, input.len(), SOFT_MAX_BLOCK_LENGTH);
            let seq_idx = 0;

            self.freqs.reset();
            self.split_stats = BlockSplitStats::new();
            self.sequences[0].litrunlen_and_length = 0;

            while pos < max_block_end && seq_idx < SEQ_STORE_LENGTH {
                choose_literal(&mut self.freqs, input[pos], &mut self.sequences[seq_idx]);
                self.split_stats.observe_literal(input[pos]);
                pos += 1;

                if self
                    .split_stats
                    .should_end_block(block_begin, pos, input.len())
                {
                    break;
                }
            }

            let block_length = pos - block_begin;
            let is_final = pos >= input.len();
            finish_block(
                os,
                &input[block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final,
            );
        }
    }
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new(CompressionLevel::default())
    }
}

/// Level 0: output uncompressed blocks only.
fn deflate_compress_none(input: &[u8], output: &mut [u8]) -> Result<usize, CompressionError> {
    if input.is_empty() {
        if output.len() < 5 {
            return Err(CompressionError::InsufficientSpace);
        }
        output[0] = 1 | (DEFLATE_BLOCKTYPE_UNCOMPRESSED << 1) as u8;
        // LEN=0, NLEN=0xFFFF
        output[1..5].copy_from_slice(&[0, 0, 0xFF, 0xFF]);
        return Ok(5);
    }

    let mut in_pos = 0;
    let mut out_pos = 0;

    while in_pos < input.len() {
        let is_last = input.len() - in_pos <= 0xFFFF;
        let len = (input.len() - in_pos).min(0xFFFF);

        if out_pos + 5 + len > output.len() {
            return Err(CompressionError::InsufficientSpace);
        }

        let bfinal = if is_last { 1u8 } else { 0 };
        output[out_pos] = bfinal | ((DEFLATE_BLOCKTYPE_UNCOMPRESSED as u8) << 1);
        out_pos += 1;

        output[out_pos..out_pos + 2].copy_from_slice(&(len as u16).to_le_bytes());
        out_pos += 2;
        output[out_pos..out_pos + 2].copy_from_slice(&(!(len as u16)).to_le_bytes());
        out_pos += 2;

        output[out_pos..out_pos + len].copy_from_slice(&input[in_pos..in_pos + len]);
        out_pos += len;
        in_pos += len;
    }

    Ok(out_pos)
}

/// Bit scan reverse: floor(log2(v)). v must be > 0.
#[inline(always)]
fn bsr32(v: u32) -> u32 {
    debug_assert!(v > 0);
    31 - v.leading_zeros()
}

/// Minimum match length lookup table indexed by number of distinct literal values.
///
/// Fewer distinct literals → longer min_match (short matches aren't worth the overhead
/// when the literal alphabet is small, e.g. DNA or binary data).
const MIN_MATCH_LEN_TABLE: [u8; 80] = [
    9, 9, 9, 9, 9, 9, 8, 8, 7, 7, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
];

/// Choose minimum match length based on literal diversity and search depth.
fn choose_min_match_len(num_used_literals: u32, max_search_depth: u32) -> u32 {
    let mut min_len = if (num_used_literals as usize) >= MIN_MATCH_LEN_TABLE.len() {
        DEFLATE_MIN_MATCH_LEN
    } else {
        MIN_MATCH_LEN_TABLE[num_used_literals as usize] as u32
    };

    // With low max_search_depth, it may be too hard to find long matches.
    if max_search_depth < 16 {
        if max_search_depth < 5 {
            min_len = min_len.min(4);
        } else if max_search_depth < 10 {
            min_len = min_len.min(5);
        } else {
            min_len = min_len.min(7);
        }
    }
    min_len
}

/// Calculate initial minimum match length by scanning literal diversity in the data.
fn calculate_min_match_len(data: &[u8], max_search_depth: u32) -> u32 {
    // For very short inputs, static Huffman has a good chance of being best.
    if data.len() < 512 {
        return DEFLATE_MIN_MATCH_LEN;
    }

    // Scan first 4 KiB to estimate literal diversity.
    let scan_len = data.len().min(4096);
    let mut used = [false; 256];
    for &b in &data[..scan_len] {
        used[b as usize] = true;
    }
    let num_used_literals = used.iter().filter(|&&u| u).count() as u32;
    choose_min_match_len(num_used_literals, max_search_depth)
}

/// Recalculate minimum match length based on actual frequency distribution.
fn recalculate_min_match_len(freqs: &DeflateFreqs, max_search_depth: u32) -> u32 {
    let literal_freq: u32 = freqs.litlen[..DEFLATE_NUM_LITERALS as usize].iter().sum();
    let cutoff = literal_freq >> 10; // Ignore rarely used literals

    let num_used_literals = freqs.litlen[..DEFLATE_NUM_LITERALS as usize]
        .iter()
        .filter(|&&f| f > cutoff)
        .count() as u32;
    choose_min_match_len(num_used_literals, max_search_depth)
}

/// Adjust max_len and nice_len when approaching the end of input.
#[inline(always)]
fn adjust_max_and_nice_len(max_len: &mut u32, nice_len: &mut u32, remaining: usize) {
    if remaining < DEFLATE_MAX_MATCH_LEN as usize {
        *max_len = remaining as u32;
        *nice_len = (*nice_len).min(*max_len);
    }
}

/// Choose the maximum block end position.
fn choose_max_block_end(block_begin: usize, in_end: usize, soft_max_len: usize) -> usize {
    if in_end - block_begin < soft_max_len + MIN_BLOCK_LENGTH {
        in_end
    } else {
        block_begin + soft_max_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_level0_empty() {
        let mut c = Compressor::new(CompressionLevel::NONE);
        let mut output = vec![0u8; 100];
        let size = c.deflate_compress(&[], &mut output).unwrap();
        assert_eq!(size, 5);

        // Decompress with our own decompressor
        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; 0];
        let dsize = d
            .deflate_decompress(&output[..size], &mut decompressed)
            .unwrap();
        assert_eq!(dsize, 0);
    }

    #[test]
    fn test_compress_level0_roundtrip() {
        let data = b"Hello, World! This is a test of uncompressed DEFLATE blocks.";
        let mut c = Compressor::new(CompressionLevel::NONE);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_level0_large() {
        let data: Vec<u8> = (0..=255).cycle().take(200_000).collect();
        let mut c = Compressor::new(CompressionLevel::NONE);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_literals_roundtrip() {
        // Test the literal-only compressor at level 6 (no matchfinding yet)
        let data = b"Hello, World! This is a test of literal-only DEFLATE compression.";
        let mut c = Compressor::new(CompressionLevel::DEFAULT);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_literals_large() {
        let data: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        let mut c = Compressor::new(CompressionLevel::DEFAULT);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        // Verify with libdeflater
        let mut d = libdeflater::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_zlib_roundtrip() {
        let data = b"Test zlib compression roundtrip!";
        let mut c = Compressor::new(CompressionLevel::DEFAULT);
        let bound = Compressor::zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(data, &mut compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .zlib_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_gzip_roundtrip() {
        let data = b"Test gzip compression roundtrip!";
        let mut c = Compressor::new(CompressionLevel::DEFAULT);
        let bound = Compressor::gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(data, &mut compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .gzip_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_cross_decompress_libdeflater() {
        // Compress with zenflate, decompress with libdeflater
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        let mut c = Compressor::new(CompressionLevel::DEFAULT);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut d = libdeflater::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    /// Helper: compress with zenflate, decompress with both zenflate and libdeflater.
    fn roundtrip_verify(data: &[u8], level: u32) {
        let mut c = Compressor::new(CompressionLevel::new(level));
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed)
            .unwrap_or_else(|e| panic!("level {level}: compress failed: {e}"));

        // Verify with our own decompressor
        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed)
            .unwrap_or_else(|e| panic!("level {level}: zenflate decompress failed: {e}"));
        assert_eq!(
            &decompressed[..dsize],
            data,
            "level {level}: zenflate roundtrip mismatch"
        );

        // Verify with libdeflater
        let mut ld = libdeflater::Decompressor::new();
        let mut ld_decompressed = vec![0u8; data.len()];
        let ld_dsize = ld
            .deflate_decompress(&compressed[..csize], &mut ld_decompressed)
            .unwrap_or_else(|e| panic!("level {level}: libdeflater decompress failed: {e}"));
        assert_eq!(
            &ld_decompressed[..ld_dsize],
            data,
            "level {level}: libdeflater roundtrip mismatch"
        );
    }

    #[test]
    fn test_fastest_small() {
        roundtrip_verify(b"Hello, World!", 1);
    }

    #[test]
    fn test_fastest_repetitive() {
        let data: Vec<u8> = b"abcabcabcabcabcabcabc".repeat(100);
        roundtrip_verify(&data, 1);
    }

    #[test]
    fn test_fastest_zeros() {
        let data = vec![0u8; 100_000];
        roundtrip_verify(&data, 1);
    }

    #[test]
    fn test_fastest_sequential() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        roundtrip_verify(&data, 1);
    }

    #[test]
    fn test_fastest_large() {
        // Mix of repetitive and varied data
        let mut data = Vec::with_capacity(500_000);
        for i in 0..500_000u32 {
            data.push(((i * 7 + 13) % 256) as u8);
        }
        roundtrip_verify(&data, 1);
    }

    #[test]
    fn test_fastest_actually_compresses() {
        // Verify level 1 actually produces smaller output than literal-only
        let data = vec![0u8; 10_000];
        let mut c = Compressor::new(CompressionLevel::FASTEST);
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();
        // All zeros should compress very well
        assert!(
            csize < data.len() / 10,
            "Level 1 should compress all-zeros well: {csize} >= {}",
            data.len() / 10
        );
    }

    #[test]
    fn test_fastest_cross_decompress_c() {
        // Compress with C libdeflate level 1, decompress with zenflate
        let data: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        let mut lc = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(1).unwrap());
        let bound = lc.deflate_compress_bound(data.len());
        let mut c_compressed = vec![0u8; bound];
        let c_csize = lc.deflate_compress(&data, &mut c_compressed).unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&c_compressed[..c_csize], &mut decompressed)
            .unwrap();
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_bound() {
        // Empty input
        assert_eq!(Compressor::deflate_compress_bound(0), 5);
        // Small input
        assert_eq!(Compressor::deflate_compress_bound(100), 105);
        // Exactly MIN_BLOCK_LENGTH
        assert_eq!(Compressor::deflate_compress_bound(5000), 5005);
        // Large input: 1MB
        let bound = Compressor::deflate_compress_bound(1_000_000);
        assert!(bound >= 1_000_000);
        assert!(bound < 1_002_000); // shouldn't be too much larger
    }

    // ---- Greedy strategy tests (levels 2-4) ----

    #[test]
    fn test_greedy_small() {
        for level in 2..=4 {
            roundtrip_verify(b"Hello, World!", level);
        }
    }

    #[test]
    fn test_greedy_repetitive() {
        let data: Vec<u8> = b"abcabcabcabcabcabcabc".repeat(100);
        for level in 2..=4 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_greedy_zeros() {
        let data = vec![0u8; 100_000];
        for level in 2..=4 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_greedy_sequential() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        for level in 2..=4 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_greedy_large() {
        let mut data = Vec::with_capacity(500_000);
        for i in 0..500_000u32 {
            data.push(((i * 7 + 13) % 256) as u8);
        }
        for level in 2..=4 {
            roundtrip_verify(&data, level);
        }
    }

    // ---- Lazy strategy tests (levels 5-7) ----

    #[test]
    fn test_lazy_small() {
        for level in 5..=7 {
            roundtrip_verify(b"Hello, World!", level);
        }
    }

    #[test]
    fn test_lazy_repetitive() {
        let data: Vec<u8> = b"abcabcabcabcabcabcabc".repeat(100);
        for level in 5..=7 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy_zeros() {
        let data = vec![0u8; 100_000];
        for level in 5..=7 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy_sequential() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        for level in 5..=7 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy_large() {
        let mut data = Vec::with_capacity(500_000);
        for i in 0..500_000u32 {
            data.push(((i * 7 + 13) % 256) as u8);
        }
        for level in 5..=7 {
            roundtrip_verify(&data, level);
        }
    }

    // ---- Lazy2 strategy tests (levels 8-9) ----

    #[test]
    fn test_lazy2_small() {
        for level in 8..=9 {
            roundtrip_verify(b"Hello, World!", level);
        }
    }

    #[test]
    fn test_lazy2_repetitive() {
        let data: Vec<u8> = b"abcabcabcabcabcabcabc".repeat(100);
        for level in 8..=9 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy2_zeros() {
        let data = vec![0u8; 100_000];
        for level in 8..=9 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy2_sequential() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        for level in 8..=9 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_lazy2_large() {
        let mut data = Vec::with_capacity(500_000);
        for i in 0..500_000u32 {
            data.push(((i * 7 + 13) % 256) as u8);
        }
        for level in 8..=9 {
            roundtrip_verify(&data, level);
        }
    }

    // ---- Near-optimal strategy tests (levels 10-12) ----

    #[test]
    fn test_near_optimal_small() {
        for level in 10..=12 {
            roundtrip_verify(b"Hello, World!", level);
        }
    }

    #[test]
    fn test_near_optimal_repetitive() {
        let data: Vec<u8> = b"abcabcabcabcabcabcabc".repeat(100);
        for level in 10..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_near_optimal_zeros() {
        let data = vec![0u8; 100_000];
        for level in 10..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_near_optimal_sequential() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        for level in 10..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_near_optimal_large() {
        let mut data = Vec::with_capacity(500_000);
        for i in 0..500_000u32 {
            data.push(((i * 7 + 13) % 256) as u8);
        }
        for level in 10..=12 {
            roundtrip_verify(&data, level);
        }
    }

    // ---- Cross-level tests ----

    #[test]
    fn test_all_levels_roundtrip() {
        // Test all levels 0-12 with the same data
        let data: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        for level in 0..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_all_levels_cross_decompress_c() {
        // Compress with C libdeflate at each level, decompress with zenflate
        let data: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        for level in 1..=12 {
            let mut lc =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
            let bound = lc.deflate_compress_bound(data.len());
            let mut c_compressed = vec![0u8; bound];
            let c_csize = lc.deflate_compress(&data, &mut c_compressed).unwrap();

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .deflate_decompress(&c_compressed[..c_csize], &mut decompressed)
                .unwrap_or_else(|e| {
                    panic!("level {level}: zenflate decompress of C output failed: {e}")
                });
            assert_eq!(
                &decompressed[..dsize],
                &data[..],
                "level {level}: C→Rust cross-decompression mismatch"
            );
        }
    }

    #[test]
    fn test_compression_improves_with_level() {
        // Higher levels should generally compress at least as well (or better)
        let data = vec![0u8; 50_000];
        let mut prev_size = None;
        for level in 1..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();
            // Allow some tolerance — strategy transitions might not always improve
            if let Some(prev) = prev_size {
                assert!(
                    csize <= prev + 100,
                    "level {level} ({csize}) much worse than level {} ({prev})",
                    level - 1
                );
            }
            prev_size = Some(csize);
        }
    }

    #[test]
    fn test_zlib_all_levels() {
        let data =
            b"Test zlib compression at all levels with sufficient input data for matchfinding.";
        let data = data.repeat(50);
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::zlib_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .zlib_compress(&data, &mut compressed)
                .unwrap_or_else(|e| panic!("level {level}: zlib compress failed: {e}"));

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .zlib_decompress(&compressed[..csize], &mut decompressed)
                .unwrap_or_else(|e| panic!("level {level}: zlib decompress failed: {e}"));
            assert_eq!(
                &decompressed[..dsize],
                &data[..],
                "level {level}: zlib roundtrip mismatch"
            );
        }
    }

    #[test]
    fn test_gzip_all_levels() {
        let data =
            b"Test gzip compression at all levels with sufficient input data for matchfinding.";
        let data = data.repeat(50);
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::gzip_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .gzip_compress(&data, &mut compressed)
                .unwrap_or_else(|e| panic!("level {level}: gzip compress failed: {e}"));

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .gzip_decompress(&compressed[..csize], &mut decompressed)
                .unwrap_or_else(|e| panic!("level {level}: gzip decompress failed: {e}"));
            assert_eq!(
                &decompressed[..dsize],
                &data[..],
                "level {level}: gzip roundtrip mismatch"
            );
        }
    }

    #[test]
    fn test_window_boundary_crossing() {
        // Data larger than the 32K matchfinder window to test window sliding
        let mut data = Vec::with_capacity(100_000);
        // Create data with repeating patterns at distances > 32K
        for i in 0..100_000u32 {
            data.push((i % 251) as u8); // prime modulus for less obvious patterns
        }
        for level in 1..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_block_splitting() {
        // Data with distinct distributions to trigger block splitting
        let mut data = Vec::with_capacity(100_000);
        // First half: low entropy (mostly zeros)
        data.extend(core::iter::repeat_n(0u8, 50_000));
        // Second half: high entropy (sequential)
        data.extend((0..=255u8).cycle().take(50_000));
        for level in 2..=12 {
            roundtrip_verify(&data, level);
        }
    }

    #[test]
    fn test_short_inputs() {
        // Test various short inputs that exercise edge cases
        for level in 1..=12 {
            roundtrip_verify(b"", level);
            roundtrip_verify(b"a", level);
            roundtrip_verify(b"ab", level);
            roundtrip_verify(b"abc", level);
            roundtrip_verify(b"abcd", level);
            roundtrip_verify(b"Hello", level);
            roundtrip_verify(&[0u8; 100], level);
        }
    }
}

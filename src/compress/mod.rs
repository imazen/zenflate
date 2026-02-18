//! DEFLATE/zlib/gzip compression.
//!
//! Ported from libdeflate's `deflate_compress.c`, `zlib_compress.c`, `gzip_compress.c`.

pub(crate) mod bitstream;
pub(crate) mod block;
pub(crate) mod block_split;
pub(crate) mod huffman;
pub(crate) mod sequences;

use crate::checksum::{adler32, crc32};
use crate::constants::*;
use crate::error::CompressionError;
use crate::matchfinder::ht::{HT_MATCHFINDER_REQUIRED_NBYTES, HtMatchfinder};
use crate::matchfinder::lz_hash;

use self::bitstream::OutputBitstream;
use self::block::{DeflateCodes, DeflateFreqs, choose_literal, choose_match, finish_block};
use self::block_split::{BlockSplitStats, MIN_BLOCK_LENGTH};
use self::sequences::Sequence;

/// Hash order for the ht_matchfinder (needed for initial hash computation).
const HT_MATCHFINDER_HASH_ORDER: u32 = 15;

/// Soft maximum block length (uncompressed bytes). Blocks are ended around here.
const SOFT_MAX_BLOCK_LENGTH: usize = 300000;

/// Maximum number of sequences for greedy/lazy/lazy2 strategies.
const SEQ_STORE_LENGTH: usize = 50000;

/// Soft maximum block length for the fastest strategy.
#[allow(dead_code)]
const FAST_SOFT_MAX_BLOCK_LENGTH: usize = 65535;

/// Maximum number of sequences for the fastest strategy.
const FAST_SEQ_STORE_LENGTH: usize = 8192;

/// Compression level (0-12).
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
            9..=12 => (600, DEFLATE_MAX_MATCH_LEN),
            _ => unreachable!(),
        };

        let max_passthrough_size = if lvl == 0 {
            usize::MAX
        } else {
            55 - (lvl as usize * 4)
        };

        let seq_capacity = if lvl == 1 {
            FAST_SEQ_STORE_LENGTH + 1
        } else if lvl >= 2 {
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
            _ => {
                // Levels 2-12: literal-only placeholder until hc/bt matchfinders are done
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
                    u32::from_le_bytes(input[in_next..in_next + 4].try_into().unwrap()),
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
}

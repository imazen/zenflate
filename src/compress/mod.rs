//! DEFLATE/zlib/gzip compression.
//!
//! Ported from libdeflate's `deflate_compress.c`, `zlib_compress.c`, `gzip_compress.c`.

pub(crate) mod bitstream;
pub(crate) mod block;
pub(crate) mod block_split;
pub(crate) mod huffman;
pub(crate) mod near_optimal;
pub(crate) mod sequences;

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, vec, vec::Vec};

use crate::checksum::{adler32, crc32};
use crate::constants::*;
use crate::error::CompressionError;
use crate::matchfinder::MATCHFINDER_WINDOW_SIZE;
use crate::matchfinder::bt::{BT_MATCHFINDER_REQUIRED_NBYTES, LzMatch};
use crate::matchfinder::fast_ht::{FAST_HT_REQUIRED_NBYTES, FastHtMatchfinder};
use crate::matchfinder::hc::HcMatchfinder;
use crate::matchfinder::ht::{HT_MATCHFINDER_REQUIRED_NBYTES, HtMatchfinder};
use crate::matchfinder::lz_hash;
use crate::matchfinder::turbo::{TURBO_REQUIRED_NBYTES, TurboMatchfinder};

use self::bitstream::OutputBitstream;
use self::block::{
    DeflateCodes, DeflateFreqs, LENGTH_SLOT, choose_literal, choose_match, finish_block,
    get_offset_slot,
};
use self::block_split::{BlockSplitStats, MIN_BLOCK_LENGTH};
use self::near_optimal::{
    MATCH_CACHE_LENGTH, NearOptimalState, clear_old_stats, init_stats, merge_stats,
    optimize_and_flush_block, save_stats,
};
use self::sequences::Sequence;

/// Hash order for the ht_matchfinder (needed for initial hash computation).
const HT_MATCHFINDER_HASH_ORDER: u32 = 15;

/// Hash order for the turbo matchfinder.
const TURBO_MF_HASH_ORDER: u32 = crate::matchfinder::turbo::TURBO_MATCHFINDER_HASH_ORDER;

/// Hash order for the fast_ht matchfinder.
const FAST_HT_MF_HASH_ORDER: u32 = crate::matchfinder::fast_ht::FAST_HT_MATCHFINDER_HASH_ORDER;

/// Soft maximum block length (uncompressed bytes). Blocks are ended around here.
const SOFT_MAX_BLOCK_LENGTH: usize = 300000;

/// Maximum number of sequences for greedy/lazy/lazy2 strategies.
const SEQ_STORE_LENGTH: usize = 50000;

/// Soft maximum block length for the fastest strategy.
const FAST_SOFT_MAX_BLOCK_LENGTH: usize = 65535;

/// Maximum number of sequences for the fastest strategy.
const FAST_SEQ_STORE_LENGTH: usize = 8192;

/// Internal compression strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InternalStrategy {
    /// No compression — store blocks only.
    Store,
    /// Static Huffman codes + turbo matchfinder (single-entry hash, limited updates).
    #[allow(dead_code)]
    StaticTurbo,
    /// Dynamic Huffman codes + turbo matchfinder (single-entry hash, limited updates).
    Turbo,
    /// 2-entry hash table + limited hash updates during skips.
    FastHt,
    /// Original 2-entry hash table with full hash updates (libdeflate level 1 compat).
    HtGreedy,
    /// Hash chain greedy matchfinder.
    Greedy,
    /// Hash chain lazy matchfinder (single lookahead).
    Lazy,
    /// Hash chain double-lazy matchfinder (two lookaheads).
    Lazy2,
    /// Binary tree near-optimal parser with iterative backward DP.
    NearOptimal,
}

/// Map effort (0-30) to internal strategy.
fn effort_to_strategy(effort: u32) -> InternalStrategy {
    match effort {
        0 => InternalStrategy::Store,
        1..=4 => InternalStrategy::Turbo,
        5..=9 => InternalStrategy::FastHt,
        10 => InternalStrategy::Greedy,
        11..=17 => InternalStrategy::Lazy,
        18..=22 => InternalStrategy::Lazy2,
        _ => InternalStrategy::NearOptimal,
    }
}

/// Parameters controlling matchfinding behavior.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompressionParams {
    pub max_search_depth: u32,
    pub nice_match_length: u32,
    /// Reduce chain search depth 4x when best match >= this length.
    /// Set to DEFLATE_MAX_MATCH_LEN + 1 to disable.
    pub good_match: u32,
    /// Skip lazy evaluation when current match >= this length.
    /// Set to DEFLATE_MAX_MATCH_LEN + 1 to disable.
    pub max_lazy: u32,
}

/// Compression level controlling the speed/ratio tradeoff.
///
/// # Named presets
///
/// | Preset | Effort | Strategy |
/// |--------|--------|----------|
/// | [`none()`](Self::none) | 0 | Store (no compression) |
/// | [`fastest()`](Self::fastest) | 1 | Turbo hash table |
/// | [`fast()`](Self::fast) | 10 | Greedy hash chains |
/// | [`balanced()`](Self::balanced) | 15 | Lazy matching (default) |
/// | [`high()`](Self::high) | 22 | Double-lazy matching |
/// | [`best()`](Self::best) | 30 | Near-optimal parsing |
///
/// # Fine-grained control
///
/// [`new(effort)`](Self::new) accepts 0-30 for intermediate tradeoffs.
/// Higher effort within a strategy increases search depth and match quality.
///
/// | Effort range | Strategy |
/// |--------------|----------|
/// | 0 | Store |
/// | 1-4 | Turbo |
/// | 5-9 | Fast HT |
/// | 10 | Greedy |
/// | 11-17 | Lazy |
/// | 18-22 | Double-lazy |
/// | 23-30 | Near-optimal |
///
/// # C libdeflate compatibility
///
/// [`libdeflate(level)`](Self::libdeflate) (0-12) produces byte-identical
/// output with C libdeflate at the given level.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// // Named presets
/// let level = CompressionLevel::balanced(); // effort 15, lazy matching
/// assert_eq!(level.effort(), 15);
///
/// // Fine-grained effort (clamped to 0-30)
/// let level = CompressionLevel::new(12); // lazy matching, mid-range depth
/// assert_eq!(level.effort(), 12);
///
/// // Byte-identical C libdeflate compatibility
/// let compat = CompressionLevel::libdeflate(6);
/// assert_eq!(compat.level(), 6);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompressionLevel {
    effort: u32,
    strategy: InternalStrategy,
    /// When Some, use exact C libdeflate parameters for byte-identical output.
    libdeflate_level: Option<u8>,
}

impl CompressionLevel {
    /// Create a compression level from an effort value (0-30). Clamps to 0-30.
    ///
    /// Higher effort = better compression ratio but slower.
    pub fn new(effort: u32) -> Self {
        let effort = effort.min(30);
        Self {
            effort,
            strategy: effort_to_strategy(effort),
            libdeflate_level: None,
        }
    }

    /// Create a compression level that produces byte-identical output with
    /// C libdeflate at the given level (0-12). Clamps to 0-12.
    pub fn libdeflate(level: u32) -> Self {
        let level = level.min(12);
        let (strategy, effort) = match level {
            0 => (InternalStrategy::Store, 0),
            1 => (InternalStrategy::HtGreedy, 4),
            2 => (InternalStrategy::Greedy, 8),
            3 => (InternalStrategy::Greedy, 9),
            4 => (InternalStrategy::Greedy, 10),
            5 => (InternalStrategy::Lazy, 11),
            6 => (InternalStrategy::Lazy, 15),
            7 => (InternalStrategy::Lazy, 17),
            8 => (InternalStrategy::Lazy2, 18),
            9 => (InternalStrategy::Lazy2, 22),
            10 => (InternalStrategy::NearOptimal, 23),
            11 => (InternalStrategy::NearOptimal, 26),
            _ => (InternalStrategy::NearOptimal, 30),
        };
        Self {
            effort,
            strategy,
            libdeflate_level: Some(level as u8),
        }
    }

    /// Get the effort level (0-30).
    pub fn effort(self) -> u32 {
        self.effort
    }

    /// Returns true if this level was created with [`libdeflate()`](Self::libdeflate)
    /// and requires byte-identical output with C libdeflate.
    pub(crate) fn is_libdeflate_compat(self) -> bool {
        self.libdeflate_level.is_some()
    }

    /// Get the approximate numeric level (0-12) for backward compatibility.
    ///
    /// For levels created with [`libdeflate()`](Self::libdeflate), returns the
    /// exact libdeflate level. For effort-based levels, returns an approximation.
    pub fn level(self) -> u32 {
        if let Some(ld) = self.libdeflate_level {
            return ld as u32;
        }
        match self.effort {
            0 => 0,
            1..=9 => 1,
            10 => 4,
            11 => 5,
            12..=15 => 6,
            16..=17 => 7,
            18..=19 => 8,
            20..=22 => 9,
            23..=25 => 10,
            26..=28 => 11,
            _ => 12,
        }
    }

    /// Internal strategy for dispatch.
    pub(crate) fn strategy(self) -> InternalStrategy {
        self.strategy
    }

    /// Effort 0: no compression. Wraps input in uncompressed DEFLATE blocks.
    pub fn none() -> Self {
        Self::new(0)
    }

    /// Effort 1: fastest compression. Turbo matchfinder with dynamic Huffman.
    pub fn fastest() -> Self {
        Self::new(1)
    }

    /// Effort 10: fast compression. Greedy hash-chain matchfinder.
    pub fn fast() -> Self {
        Self::new(10)
    }

    /// Effort 15: balanced compression. Lazy hash-chain matchfinder.
    /// This is the default.
    pub fn balanced() -> Self {
        Self::new(15)
    }

    /// Effort 22: high compression. Double-lazy hash-chain matchfinder.
    /// Best ratio before the much slower near-optimal parser.
    pub fn high() -> Self {
        Self::new(22)
    }

    /// Effort 30: maximum compression. Near-optimal parser with multiple passes.
    pub fn best() -> Self {
        Self::new(30)
    }

    /// Returns a fallback level to test for monotonicity across strategies.
    ///
    /// Different compression strategies (FastHt, Greedy, Lazy, etc.) use
    /// fundamentally different algorithms. A more sophisticated algorithm
    /// can produce *larger* output than a simpler one on some data types,
    /// even at higher effort.
    ///
    /// When this returns `Some(fallback)`, callers wanting monotonic output
    /// should compress with both `self` and `fallback`, keeping the smaller
    /// result.
    ///
    /// The fallback chain can be followed for deeper guarantees — each
    /// link points to the previous strategy's maximum effort:
    /// ```
    /// # use zenflate::CompressionLevel;
    /// let level = CompressionLevel::new(15); // Lazy
    /// let mut chain = vec![level];
    /// let mut cur = level;
    /// while let Some(fb) = cur.monotonicity_fallback() {
    ///     chain.push(fb);
    ///     cur = fb;
    /// }
    /// // chain = [e15 (Lazy), e10 (Greedy), e9 (FastHt)]
    /// assert_eq!(chain.len(), 3);
    /// ```
    ///
    /// This does NOT cover within-strategy butterfly effects (small,
    /// typically <0.01% of input size). For absolute monotonicity,
    /// callers should track the running minimum across all effort levels.
    pub fn monotonicity_fallback(&self) -> Option<CompressionLevel> {
        if self.libdeflate_level.is_some() {
            return None;
        }
        // Each strategy's levels fall back to the previous strategy's max.
        // The chain terminates at FastHt (Turbo→FastHt always improves).
        match self.effort {
            10 => Some(Self::new(9)),       // Greedy → FastHt max
            11..=17 => Some(Self::new(10)), // Lazy → Greedy max
            18..=22 => Some(Self::new(17)), // Lazy2 → Lazy max
            23..=30 => Some(Self::new(22)), // NearOptimal → Lazy2 max
            _ => None,
        }
    }

    /// Returns compression parameters for Compressor initialization.
    pub(crate) fn compression_params(self) -> CompressionParams {
        // Value that effectively disables the feature (max match len + 1).
        const DISABLED: u32 = DEFLATE_MAX_MATCH_LEN + 1;

        if let Some(ld) = self.libdeflate_level {
            let (depth, nice) = match ld {
                0 => (0, 0),
                1 => (0, 32),
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
                _ => (300, DEFLATE_MAX_MATCH_LEN),
            };
            return CompressionParams {
                max_search_depth: depth,
                nice_match_length: nice,
                good_match: DISABLED,
                max_lazy: DISABLED,
            };
        }

        let (depth, nice) = match self.strategy {
            InternalStrategy::Store => (0, 0),
            InternalStrategy::StaticTurbo
            | InternalStrategy::Turbo
            | InternalStrategy::FastHt
            | InternalStrategy::HtGreedy => match self.effort {
                0..=1 => (0, 16),
                2..=4 => (0, 32),
                5 => (0, 16),
                6 => (0, 24),
                7 => (0, 32),
                8 => (0, 64),
                _ => (0, 128),
            },
            InternalStrategy::Greedy => match self.effort {
                0..=8 => (6, 10),
                9 => (12, 14),
                _ => (16, 30),
            },
            InternalStrategy::Lazy => match self.effort {
                0..=11 => (16, 30),
                12 => (20, 40),
                13 => (35, 65),
                14 => (50, 80),
                15 => (65, 100),
                16 => (80, 115),
                _ => (100, 130),
            },
            InternalStrategy::Lazy2 => match self.effort {
                0..=18 => (300, DEFLATE_MAX_MATCH_LEN),
                19 => (350, DEFLATE_MAX_MATCH_LEN),
                20 => (400, DEFLATE_MAX_MATCH_LEN),
                21 => (500, DEFLATE_MAX_MATCH_LEN),
                _ => (600, DEFLATE_MAX_MATCH_LEN),
            },
            InternalStrategy::NearOptimal => match self.effort {
                0..=23 => (35, 75),
                24 => (60, 100),
                25 => (100, 150),
                26 => (100, 150),
                27 => (125, 200),
                28 => (150, DEFLATE_MAX_MATCH_LEN),
                29 => (200, DEFLATE_MAX_MATCH_LEN),
                _ => (300, DEFLATE_MAX_MATCH_LEN),
            },
        };

        let (good_match, max_lazy) = match self.strategy {
            InternalStrategy::Greedy => match self.effort {
                0..=8 => (4, DISABLED),
                9 => (5, DISABLED),
                _ => (6, DISABLED),
            },
            InternalStrategy::Lazy => match self.effort {
                0..=11 => (6, 6),
                12 => (8, 10),
                13 => (10, 18),
                14 => (14, 32),
                15 => (32, 64),
                16 => (64, 128),
                _ => (128, DEFLATE_MAX_MATCH_LEN),
            },
            InternalStrategy::Lazy2 => match self.effort {
                0..=18 => (64, 64),
                19 => (96, 96),
                20 => (128, 128),
                _ => (DISABLED, DISABLED),
            },
            // Not used by other strategies
            _ => (DISABLED, DISABLED),
        };

        CompressionParams {
            max_search_depth: depth,
            nice_match_length: nice,
            good_match,
            max_lazy,
        }
    }

    /// Returns (passes, improvement_threshold, nonfinal_threshold, static_opt_threshold)
    /// for near-optimal compression.
    pub(crate) fn near_optimal_params(self) -> (u32, u32, u32, u32) {
        if let Some(ld) = self.libdeflate_level {
            return match ld {
                10 => (2, 32, 32, 0),
                11 => (4, 16, 16, 1000),
                _ => (10, 1, 1, 10000),
            };
        }
        match self.effort {
            0..=25 => (2, 32, 32, 0),
            26..=28 => (4, 16, 16, 1000),
            _ => (10, 1, 1, 10000),
        }
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::balanced()
    }
}

/// DEFLATE/zlib/gzip compressor.
///
/// Reuse across multiple compressions for best performance (avoids re-initialization).
///
/// ```
/// use zenflate::{Compressor, CompressionLevel, Unstoppable};
///
/// let mut compressor = Compressor::new(CompressionLevel::balanced());
///
/// let data = b"Hello, World! Hello, World! Hello, World!";
/// let bound = Compressor::deflate_compress_bound(data.len());
/// let mut out = vec![0u8; bound];
/// let size = compressor.deflate_compress(data, &mut out, Unstoppable).unwrap();
/// assert!(size < data.len()); // compressed
/// ```
pub struct Compressor {
    /// Compression level.
    level: CompressionLevel,
    /// Maximum search depth for matchfinding.
    max_search_depth: u32,
    /// "Nice" match length: stop searching if we find a match this long.
    nice_match_length: u32,
    /// Reduce chain search 4x when best match >= this length.
    good_match: u32,
    /// Skip lazy evaluation when current match >= this length.
    max_lazy: u32,
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
    /// Turbo matchfinder for StaticTurbo/Turbo strategies.
    turbo_mf: Option<Box<TurboMatchfinder>>,
    /// FastHt matchfinder for the FastHt strategy.
    fast_ht_mf: Option<Box<FastHtMatchfinder>>,
    /// Hash table matchfinder for HtGreedy strategy (libdeflate L1 compat).
    ht_mf: Option<Box<HtMatchfinder>>,
    /// Hash chains matchfinder for Greedy/Lazy/Lazy2 strategies.
    hc_mf: Option<Box<HcMatchfinder>>,
    /// Near-optimal state for the NearOptimal strategy.
    near_optimal: Option<Box<NearOptimalState>>,
    /// Starting offset: skip dictionary bytes at the start of input.
    /// Set by `deflate_compress_chunk`; 0 for normal operation.
    chunk_start: usize,
    /// Force all blocks to BFINAL=0 (for parallel non-last chunks).
    force_nonfinal: bool,
    /// Incremental compression: how far into the accumulated buffer we've compressed.
    /// 0 means no prior incremental call (matchfinder needs init).
    incremental_pos: usize,
    /// Incremental compression: matchfinder base offset (for window sliding).
    incremental_base_offset: usize,
}

/// Snapshot of compressor state for cheap save/restore during incremental compression.
///
/// Contains only the mutable state that changes between incremental calls:
/// matchfinder hash tables, frequency counters, Huffman codes, and cursor position.
/// Immutable configuration (level, parameters, static codes) is not included,
/// making this cheaper than a full [`Compressor::clone()`].
///
/// Used for filter evaluation in PNG optimization: snapshot before trying a filter,
/// restore after to try a different one.
///
/// # Example
///
/// ```no_run
/// # use zenflate::{Compressor, CompressionLevel};
/// let mut compressor = Compressor::new(CompressionLevel::fast());
/// // ... compress some rows ...
/// let snap = compressor.snapshot();
/// // ... try filter A, measure cost ...
/// compressor.restore(snap);
/// // ... try filter B from the same starting state ...
/// ```
pub struct CompressorSnapshot {
    freqs: DeflateFreqs,
    split_stats: BlockSplitStats,
    codes: DeflateCodes,
    ht_mf: Option<Box<HtMatchfinder>>,
    hc_mf: Option<Box<HcMatchfinder>>,
    incremental_pos: usize,
    incremental_base_offset: usize,
}

impl Clone for CompressorSnapshot {
    fn clone(&self) -> Self {
        Self {
            freqs: self.freqs.clone(),
            split_stats: self.split_stats.clone(),
            codes: self.codes.clone(),
            ht_mf: self.ht_mf.as_ref().map(|b| Box::new((**b).clone())),
            hc_mf: self.hc_mf.as_ref().map(|b| Box::new((**b).clone())),
            incremental_pos: self.incremental_pos,
            incremental_base_offset: self.incremental_base_offset,
        }
    }
}

impl Clone for Compressor {
    fn clone(&self) -> Self {
        Self {
            level: self.level,
            max_search_depth: self.max_search_depth,
            nice_match_length: self.nice_match_length,
            good_match: self.good_match,
            max_lazy: self.max_lazy,
            max_passthrough_size: self.max_passthrough_size,
            freqs: self.freqs.clone(),
            split_stats: self.split_stats.clone(),
            codes: self.codes.clone(),
            static_codes: self.static_codes.clone(),
            sequences: self.sequences.clone(),
            turbo_mf: self.turbo_mf.as_ref().map(|b| Box::new((**b).clone())),
            fast_ht_mf: self.fast_ht_mf.as_ref().map(|b| Box::new((**b).clone())),
            ht_mf: self.ht_mf.as_ref().map(|b| Box::new((**b).clone())),
            hc_mf: self.hc_mf.as_ref().map(|b| Box::new((**b).clone())),
            near_optimal: self.near_optimal.as_ref().map(|b| Box::new((**b).clone())),
            chunk_start: self.chunk_start,
            force_nonfinal: self.force_nonfinal,
            incremental_pos: self.incremental_pos,
            incremental_base_offset: self.incremental_base_offset,
        }
    }
}

impl Compressor {
    /// Create a new compressor at the given compression level.
    #[cfg(feature = "alloc")]
    pub fn new(level: CompressionLevel) -> Self {
        let strategy = level.strategy();
        let params = level.compression_params();
        let approx_level = level.level();

        let max_passthrough_size = if strategy == InternalStrategy::Store {
            usize::MAX
        } else {
            55usize.saturating_sub(approx_level as usize * 4)
        };

        let seq_capacity = match strategy {
            InternalStrategy::Store
            | InternalStrategy::StaticTurbo
            | InternalStrategy::NearOptimal => 0,
            InternalStrategy::Turbo | InternalStrategy::FastHt | InternalStrategy::HtGreedy => {
                FAST_SEQ_STORE_LENGTH + 1
            }
            InternalStrategy::Greedy | InternalStrategy::Lazy | InternalStrategy::Lazy2 => {
                SEQ_STORE_LENGTH + 1
            }
        };

        let mut freqs = DeflateFreqs::default();
        let mut static_codes = DeflateCodes::default();
        block::init_static_codes(&mut freqs, &mut static_codes);
        freqs.reset();

        Self {
            level,
            max_search_depth: params.max_search_depth,
            nice_match_length: params.nice_match_length,
            good_match: params.good_match,
            max_lazy: params.max_lazy,
            max_passthrough_size,
            freqs,
            split_stats: BlockSplitStats::new(),
            codes: DeflateCodes::default(),
            static_codes,
            sequences: alloc::vec![Sequence::default(); seq_capacity],
            turbo_mf: match strategy {
                InternalStrategy::StaticTurbo | InternalStrategy::Turbo => {
                    Some(Box::new(TurboMatchfinder::new()))
                }
                _ => None,
            },
            fast_ht_mf: if strategy == InternalStrategy::FastHt {
                Some(Box::new(FastHtMatchfinder::new()))
            } else {
                None
            },
            ht_mf: if strategy == InternalStrategy::HtGreedy {
                Some(Box::new(HtMatchfinder::new()))
            } else {
                None
            },
            hc_mf: match strategy {
                InternalStrategy::Greedy | InternalStrategy::Lazy | InternalStrategy::Lazy2 => {
                    Some(Box::new(HcMatchfinder::new()))
                }
                _ => None,
            },
            near_optimal: if strategy == InternalStrategy::NearOptimal {
                let (passes, improvement, nonfinal, static_opt) = level.near_optimal_params();
                Some(NearOptimalState::new(
                    passes,
                    improvement,
                    nonfinal,
                    static_opt,
                ))
            } else {
                None
            },
            chunk_start: 0,
            force_nonfinal: false,
            incremental_pos: 0,
            incremental_base_offset: 0,
        }
    }

    /// Save the current compressor state for later restoration.
    ///
    /// Returns a [`CompressorSnapshot`] containing only the mutable state
    /// (matchfinder, frequencies, codes, cursor). This is cheaper than
    /// [`Compressor::clone()`] because it skips immutable configuration,
    /// static codes, and the sequence buffer.
    ///
    /// Only meaningful for incremental compression (HtGreedy, Greedy, Lazy, Lazy2).
    pub fn snapshot(&self) -> CompressorSnapshot {
        CompressorSnapshot {
            freqs: self.freqs.clone(),
            split_stats: self.split_stats.clone(),
            codes: self.codes.clone(),
            ht_mf: self.ht_mf.as_ref().map(|b| Box::new((**b).clone())),
            hc_mf: self.hc_mf.as_ref().map(|b| Box::new((**b).clone())),
            incremental_pos: self.incremental_pos,
            incremental_base_offset: self.incremental_base_offset,
        }
    }

    /// Restore compressor state from a previously saved snapshot.
    ///
    /// After restoration, the compressor behaves as if the intervening
    /// operations never happened. The snapshot must have been created from
    /// a compressor with the same configuration (level, strategy).
    pub fn restore(&mut self, snap: CompressorSnapshot) {
        self.freqs = snap.freqs;
        self.split_stats = snap.split_stats;
        self.codes = snap.codes;
        if let Some(mf) = snap.ht_mf {
            self.ht_mf = Some(mf);
        }
        if let Some(mf) = snap.hc_mf {
            self.hc_mf = Some(mf);
        }
        self.incremental_pos = snap.incremental_pos;
        self.incremental_base_offset = snap.incremental_base_offset;
    }

    /// Compress data in raw DEFLATE format.
    pub fn deflate_compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        if input.len() <= self.max_passthrough_size {
            return deflate_compress_none(input, output);
        }

        let mut os = OutputBitstream::new(output);

        match self.level.strategy() {
            InternalStrategy::Store => {
                return deflate_compress_none(input, output);
            }
            InternalStrategy::StaticTurbo => {
                self.compress_static_turbo(&mut os, input, &stop)?;
            }
            InternalStrategy::Turbo => {
                self.compress_turbo(&mut os, input, &stop)?;
            }
            InternalStrategy::FastHt => {
                self.compress_fast_ht(&mut os, input, &stop)?;
            }
            InternalStrategy::HtGreedy => {
                self.compress_fastest(&mut os, input, &stop)?;
            }
            InternalStrategy::Greedy => {
                self.compress_greedy(&mut os, input, &stop)?;
            }
            InternalStrategy::Lazy => {
                self.compress_lazy_generic(&mut os, input, false, &stop)?;
            }
            InternalStrategy::Lazy2 => {
                self.compress_lazy_generic(&mut os, input, true, &stop)?;
            }
            InternalStrategy::NearOptimal => {
                self.compress_near_optimal(&mut os, input, &stop)?;
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
        stop: impl enough::Stop,
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

        let compressed_size = self.deflate_compress(input, &mut output[2..], stop)?;
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
        stop: impl enough::Stop,
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

        let compressed_size = self.deflate_compress(input, &mut output[10..], stop)?;
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
        // Worst case: uncompressed blocks (5 bytes overhead each).
        // Static Huffman blocks roll back to uncompressed if they expand.
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

    /// Feed data incrementally and get compressed DEFLATE output.
    ///
    /// Each call passes the FULL accumulated input (all prior data plus new data).
    /// The compressor remembers how far it has compressed (via `incremental_pos`)
    /// and only compresses the new portion, using matchfinder state from prior calls.
    ///
    /// `is_final`: set true on the last chunk to emit the DEFLATE end marker.
    ///
    /// This is designed for the forking brute-force use case: feed one PNG row
    /// at a time, fork (clone) before each row, try different filters, pick
    /// the smallest output, continue from the winner.
    ///
    /// **Constraints:**
    /// - `data` must be a superset of the data from the previous call (same prefix,
    ///   with new bytes appended).
    /// - Total data must fit in the matchfinder window (32 KB for L1-L9).
    ///   For data exceeding the window, the matchfinder silently handles sliding.
    /// - Levels 0 and 10-12 are not supported; returns `InsufficientSpace` error.
    ///
    /// Returns the number of bytes written to `output`.
    pub fn deflate_compress_incremental(
        &mut self,
        data: &[u8],
        output: &mut [u8],
        is_final: bool,
        stop: impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        let new_start = self.incremental_pos;
        if new_start >= data.len() && !is_final {
            return Ok(0); // nothing new to compress
        }

        let mut os = OutputBitstream::new(output);

        match self.level.strategy() {
            InternalStrategy::HtGreedy => {
                self.compress_incremental_ht(&mut os, data, new_start, is_final, &stop)?;
            }
            InternalStrategy::Greedy | InternalStrategy::Lazy | InternalStrategy::Lazy2 => {
                self.compress_incremental_hc(&mut os, data, new_start, is_final, &stop)?;
            }
            _ => {
                // StaticTurbo/Turbo/FastHt/Store/NearOptimal not supported incrementally
                return Err(CompressionError::InsufficientSpace);
            }
        }

        if os.overflow {
            return Err(CompressionError::InsufficientSpace);
        }

        // Write final partial byte
        if os.bitcount > 0 {
            if os.pos < os.buf.len() {
                os.buf[os.pos] = os.bitbuf as u8;
                os.pos += 1;
            } else {
                return Err(CompressionError::InsufficientSpace);
            }
        }

        self.incremental_pos = data.len();
        Ok(os.pos)
    }

    /// Reset incremental state so the next `deflate_compress_incremental` call
    /// starts fresh (reinitializes the matchfinder).
    pub fn incremental_reset(&mut self) {
        self.incremental_pos = 0;
        self.incremental_base_offset = 0;
    }

    /// Returns the current incremental cursor position.
    pub fn incremental_pos(&self) -> usize {
        self.incremental_pos
    }

    /// Estimate the compressed bit cost of new data without producing output.
    ///
    /// Runs LZ77 matching on the new portion of `data` (from `incremental_pos`
    /// to the end) and accumulates an estimated bit cost based on Huffman code
    /// lengths. Much faster than [`deflate_compress_incremental`](Self::deflate_compress_incremental)
    /// because it skips Huffman tree construction, block flushing, and bitstream encoding.
    ///
    /// **Important:** This modifies matchfinder state just like normal incremental
    /// compression. Use [`snapshot`](Self::snapshot)/[`restore`](Self::restore) to
    /// evaluate multiple candidates from the same starting state.
    ///
    /// The cost model uses code lengths from the most recent compressed block.
    /// If no block has been compressed yet, DEFLATE fixed code lengths are used.
    ///
    /// Returns the estimated bit cost as a `u64`.
    pub fn deflate_estimate_cost_incremental(
        &mut self,
        data: &[u8],
        stop: impl enough::Stop,
    ) -> Result<u64, CompressionError> {
        let new_start = self.incremental_pos;
        if new_start >= data.len() {
            return Ok(0);
        }

        // Copy code lengths locally to avoid borrow conflict with &mut self.
        // Use codes from previous block if available, otherwise static codes.
        let has_dynamic = self.codes.lens_litlen[256] > 0;
        let lens_litlen: [u8; DEFLATE_NUM_LITLEN_SYMS as usize] = if has_dynamic {
            self.codes.lens_litlen
        } else {
            self.static_codes.lens_litlen
        };
        let lens_offset: [u8; DEFLATE_NUM_OFFSET_SYMS as usize] = if has_dynamic {
            self.codes.lens_offset
        } else {
            self.static_codes.lens_offset
        };

        let cost = match self.level.strategy() {
            InternalStrategy::HtGreedy => self.estimate_cost_incremental_ht(
                data,
                new_start,
                &lens_litlen,
                &lens_offset,
                &stop,
            )?,
            InternalStrategy::Greedy | InternalStrategy::Lazy | InternalStrategy::Lazy2 => self
                .estimate_cost_incremental_hc(data, new_start, &lens_litlen, &lens_offset, &stop)?,
            _ => {
                return Err(CompressionError::InsufficientSpace);
            }
        };

        self.incremental_pos = data.len();
        Ok(cost)
    }

    /// Cost estimation using the hash table matchfinder (HtGreedy strategy).
    fn estimate_cost_incremental_ht(
        &mut self,
        input: &[u8],
        new_start: usize,
        lens_litlen: &[u8],
        lens_offset: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<u64, CompressionError> {
        let mut mf = self.ht_mf.take().unwrap();

        if new_start == 0 {
            mf.init();
            self.incremental_base_offset = 0;
        }

        let in_end = input.len();
        let mut in_next = new_start;
        let mut in_base_offset = self.incremental_base_offset;
        let mut cost = 0u64;

        if in_next < in_end {
            stop.check()?;

            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(
                    crate::fast_bytes::load_u32_le(input, in_next),
                    HT_MATCHFINDER_HASH_ORDER,
                )
            } else {
                0
            };

            while in_next < in_end {
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
                        // Match cost: length symbol + extra bits + offset symbol + extra bits
                        let len_slot = LENGTH_SLOT[length as usize] as usize;
                        let off_slot = get_offset_slot(offset) as usize;
                        cost += lens_litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] as u64;
                        cost += DEFLATE_LENGTH_EXTRA_BITS[len_slot] as u64;
                        cost += lens_offset[off_slot] as u64;
                        cost += DEFLATE_OFFSET_EXTRA_BITS[off_slot] as u64;

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

                // Literal cost
                cost += lens_litlen[input[in_next] as usize] as u64;
                in_next += 1;
            }
        }

        self.incremental_base_offset = in_base_offset;
        self.ht_mf = Some(mf);
        Ok(cost)
    }

    /// Cost estimation using the hash chains matchfinder (Greedy/Lazy/Lazy2 strategies).
    fn estimate_cost_incremental_hc(
        &mut self,
        input: &[u8],
        new_start: usize,
        lens_litlen: &[u8],
        lens_offset: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<u64, CompressionError> {
        let mut mf = self.hc_mf.take().unwrap();

        if new_start == 0 {
            mf.init();
            self.incremental_base_offset = 0;
        }

        let in_end = input.len();
        let mut in_next = new_start;
        let mut in_base_offset = self.incremental_base_offset;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let max_search_depth = self.max_search_depth;
        let good_match = self.good_match;
        let max_lazy = self.max_lazy;

        let mut next_hashes = [0u32; 2];
        let mut cost = 0u64;

        let min_len = if in_next < in_end {
            calculate_min_match_len(
                &input[in_next..in_end.min(in_next + SOFT_MAX_BLOCK_LENGTH)],
                max_search_depth,
            )
        } else {
            DEFLATE_MIN_MATCH_LEN
        };

        if in_next < in_end {
            stop.check()?;

            if matches!(
                self.level.strategy(),
                InternalStrategy::Lazy | InternalStrategy::Lazy2
            ) {
                // Lazy path with cost estimation
                loop {
                    adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                    let (mut cur_len, mut cur_offset) = mf.longest_match(
                        input,
                        &mut in_base_offset,
                        in_next,
                        min_len - 1,
                        max_len,
                        nice_len,
                        max_search_depth,
                        good_match,
                        &mut next_hashes,
                    );

                    if cur_len < min_len || (cur_len == DEFLATE_MIN_MATCH_LEN && cur_offset > 8192)
                    {
                        cost += lens_litlen[input[in_next] as usize] as u64;
                        in_next += 1;
                    } else {
                        in_next += 1;
                        loop {
                            if cur_len >= nice_len || cur_len >= max_lazy {
                                let len_slot = LENGTH_SLOT[cur_len as usize] as usize;
                                let off_slot = get_offset_slot(cur_offset) as usize;
                                cost +=
                                    lens_litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] as u64;
                                cost += DEFLATE_LENGTH_EXTRA_BITS[len_slot] as u64;
                                cost += lens_offset[off_slot] as u64;
                                cost += DEFLATE_OFFSET_EXTRA_BITS[off_slot] as u64;

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

                            adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                            let (next_len, next_offset) = mf.longest_match(
                                input,
                                &mut in_base_offset,
                                in_next,
                                cur_len - 1,
                                max_len,
                                nice_len,
                                max_search_depth >> 1,
                                good_match,
                                &mut next_hashes,
                            );
                            in_next += 1;

                            if next_len >= cur_len
                                && 4 * (next_len as i32 - cur_len as i32)
                                    + (bsr32(cur_offset) as i32 - bsr32(next_offset) as i32)
                                    > 2
                            {
                                cost += lens_litlen[input[in_next - 2] as usize] as u64;
                                cur_len = next_len;
                                cur_offset = next_offset;
                                continue;
                            }

                            let len_slot = LENGTH_SLOT[cur_len as usize] as usize;
                            let off_slot = get_offset_slot(cur_offset) as usize;
                            cost += lens_litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] as u64;
                            cost += DEFLATE_LENGTH_EXTRA_BITS[len_slot] as u64;
                            cost += lens_offset[off_slot] as u64;
                            cost += DEFLATE_OFFSET_EXTRA_BITS[off_slot] as u64;

                            let skip = if self.level.strategy() == InternalStrategy::Lazy2 {
                                cur_len.saturating_sub(3)
                            } else {
                                cur_len - 2
                            };
                            if skip > 0 {
                                mf.skip_bytes(
                                    input,
                                    &mut in_base_offset,
                                    in_next,
                                    in_end,
                                    skip,
                                    &mut next_hashes,
                                );
                                in_next += skip as usize;
                            }
                            break;
                        }
                    }

                    if in_next >= in_end {
                        break;
                    }
                }
            } else {
                // Greedy path with cost estimation
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
                        good_match,
                        &mut next_hashes,
                    );

                    if length >= min_len && (length > DEFLATE_MIN_MATCH_LEN || offset <= 4096) {
                        let len_slot = LENGTH_SLOT[length as usize] as usize;
                        let off_slot = get_offset_slot(offset) as usize;
                        cost += lens_litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] as u64;
                        cost += DEFLATE_LENGTH_EXTRA_BITS[len_slot] as u64;
                        cost += lens_offset[off_slot] as u64;
                        cost += DEFLATE_OFFSET_EXTRA_BITS[off_slot] as u64;

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
                        cost += lens_litlen[input[in_next] as usize] as u64;
                        in_next += 1;
                    }

                    if in_next >= in_end {
                        break;
                    }
                }
            }
        }

        self.incremental_base_offset = in_base_offset;
        self.hc_mf = Some(mf);
        Ok(cost)
    }

    /// Incremental compression using the hash table matchfinder (L1).
    ///
    /// On first call (new_start == 0), initializes the matchfinder.
    /// On subsequent calls, uses existing matchfinder state — hash entries
    /// from prior rows provide context for matching.
    fn compress_incremental_ht(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        new_start: usize,
        is_final: bool,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.ht_mf.take().unwrap();

        if new_start == 0 {
            mf.init();
            self.incremental_base_offset = 0;
        }

        let in_end = input.len();
        let mut in_next = new_start;
        let mut in_base_offset = self.incremental_base_offset;

        // Feed new bytes to the matchfinder and produce one DEFLATE block.
        if in_next < in_end {
            stop.check()?;
            let in_block_begin = in_next;
            let mut seq_idx = 0;

            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(
                    crate::fast_bytes::load_u32_le(input, in_next),
                    HT_MATCHFINDER_HASH_ORDER,
                )
            } else {
                0
            };

            while in_next < in_end && seq_idx < FAST_SEQ_STORE_LENGTH {
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
            finish_block(
                os,
                &input[in_block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final && in_next >= in_end,
            );
        }

        self.incremental_base_offset = in_base_offset;
        self.ht_mf = Some(mf);
        Ok(())
    }

    /// Incremental compression using the hash chains matchfinder (L2-L9).
    ///
    /// On first call (new_start == 0), initializes the matchfinder.
    /// On subsequent calls, uses existing matchfinder state.
    fn compress_incremental_hc(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        new_start: usize,
        is_final: bool,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.hc_mf.take().unwrap();

        if new_start == 0 {
            mf.init();
            self.incremental_base_offset = 0;
        }

        let in_end = input.len();
        let mut in_next = new_start;
        let mut in_base_offset = self.incremental_base_offset;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let max_search_depth = self.max_search_depth;
        let good_match = self.good_match;
        let max_lazy = self.max_lazy;
        let lazy2 = self.level.strategy() == InternalStrategy::Lazy2;

        let mut next_hashes = [0u32; 2];

        // Feed new bytes and produce one DEFLATE block.
        if in_next < in_end {
            stop.check()?;
            let in_block_begin = in_next;
            let mut seq_idx = 0;

            self.split_stats = BlockSplitStats::new();
            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let min_len = calculate_min_match_len(
                &input[in_next..in_end.min(in_next + SOFT_MAX_BLOCK_LENGTH)],
                max_search_depth,
            );

            if matches!(
                self.level.strategy(),
                InternalStrategy::Lazy | InternalStrategy::Lazy2
            ) {
                // Lazy/lazy2 path
                loop {
                    adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                    let (mut cur_len, mut cur_offset) = mf.longest_match(
                        input,
                        &mut in_base_offset,
                        in_next,
                        min_len - 1,
                        max_len,
                        nice_len,
                        max_search_depth,
                        good_match,
                        &mut next_hashes,
                    );

                    if cur_len < min_len || (cur_len == DEFLATE_MIN_MATCH_LEN && cur_offset > 8192)
                    {
                        choose_literal(
                            &mut self.freqs,
                            input[in_next],
                            &mut self.sequences[seq_idx],
                        );
                        self.split_stats.observe_literal(input[in_next]);
                        in_next += 1;
                    } else {
                        in_next += 1;
                        loop {
                            if cur_len >= nice_len || cur_len >= max_lazy {
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

                            adjust_max_and_nice_len(&mut max_len, &mut nice_len, in_end - in_next);
                            let (next_len, next_offset) = mf.longest_match(
                                input,
                                &mut in_base_offset,
                                in_next,
                                cur_len - 1,
                                max_len,
                                nice_len,
                                max_search_depth >> 1,
                                good_match,
                                &mut next_hashes,
                            );
                            in_next += 1;

                            if next_len >= cur_len
                                && 4 * (next_len as i32 - cur_len as i32)
                                    + (bsr32(cur_offset) as i32 - bsr32(next_offset) as i32)
                                    > 2
                            {
                                choose_literal(
                                    &mut self.freqs,
                                    input[in_next - 2],
                                    &mut self.sequences[seq_idx],
                                );
                                self.split_stats.observe_literal(input[in_next - 2]);
                                cur_len = next_len;
                                cur_offset = next_offset;
                                continue;
                            }

                            seq_idx = choose_match(
                                &mut self.freqs,
                                cur_len,
                                cur_offset,
                                &mut self.sequences,
                                seq_idx,
                            );
                            self.split_stats.observe_match(cur_len);
                            let skip = if lazy2 {
                                cur_len.saturating_sub(3)
                            } else {
                                cur_len - 2
                            };
                            if skip > 0 {
                                mf.skip_bytes(
                                    input,
                                    &mut in_base_offset,
                                    in_next,
                                    in_end,
                                    skip,
                                    &mut next_hashes,
                                );
                                in_next += skip as usize;
                            }
                            break;
                        }
                    }

                    if in_next >= in_end || seq_idx >= SEQ_STORE_LENGTH {
                        break;
                    }
                }
            } else {
                // Greedy path (L2-L4)
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
                        good_match,
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

                    if in_next >= in_end || seq_idx >= SEQ_STORE_LENGTH {
                        break;
                    }
                }
            }

            let block_length = in_next - in_block_begin;
            finish_block(
                os,
                &input[in_block_begin..],
                block_length,
                &self.sequences[..=seq_idx],
                &mut self.freqs,
                &mut self.codes,
                &self.static_codes,
                is_final && in_next >= in_end,
            );
        }

        self.incremental_base_offset = in_base_offset;
        self.hc_mf = Some(mf);
        Ok(())
    }

    /// Level 1: fastest compression using hash table matchfinder.
    ///
    /// Simple greedy: find longest match, take it or emit literal.
    /// No block splitting (uses fixed FAST_SOFT_MAX_BLOCK_LENGTH).
    fn compress_fastest(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.ht_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;

        // Dictionary warm-up: seed hash table with positions before chunk_start
        if self.chunk_start > 0 && in_next + 4 <= in_end {
            let mut warmup_hash = lz_hash(
                crate::fast_bytes::load_u32_le(input, 0),
                HT_MATCHFINDER_HASH_ORDER,
            );
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                self.chunk_start as u32,
                &mut warmup_hash,
            );
        }

        while in_next < in_end && !os.overflow {
            stop.check()?;
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
            let is_final = !self.force_nonfinal && in_next >= in_end;
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
        Ok(())
    }

    /// Static Huffman + turbo matchfinder: fastest compression.
    ///
    /// Emits RFC 1951 fixed Huffman codes inline during matching — no sequence
    /// buffer, no histogram, no tree construction or serialization.
    /// Uses the turbo matchfinder (single-entry hash, limited skip updates).
    ///
    /// If a static Huffman block would expand the data, rolls back the output
    /// and emits an uncompressed block instead (never expands).
    fn compress_static_turbo(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        use crate::fast_bytes::load_u32_le;

        let mut mf = self.turbo_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;

        // Dictionary warm-up
        if self.chunk_start > 0 && in_next + 4 <= in_end {
            let mut warmup_hash = lz_hash(load_u32_le(input, 0), TURBO_MF_HASH_ORDER);
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                self.chunk_start as u32,
                &mut warmup_hash,
            );
        }

        // Precompute combined length codewords for static Huffman.
        let sc = &self.static_codes;
        let mut full_len_cw = [0u32; DEFLATE_MAX_MATCH_LEN as usize + 1];
        let mut full_len_bits = [0u8; DEFLATE_MAX_MATCH_LEN as usize + 1];
        for mlen in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
            let slot = LENGTH_SLOT[mlen as usize] as usize;
            let sym = DEFLATE_FIRST_LEN_SYM as usize + slot;
            let extra = mlen - DEFLATE_LENGTH_BASE[slot] as u32;
            full_len_cw[mlen as usize] = sc.codewords_litlen[sym] | (extra << sc.lens_litlen[sym]);
            full_len_bits[mlen as usize] = sc.lens_litlen[sym] + DEFLATE_LENGTH_EXTRA_BITS[slot];
        }

        let nice_len = self.nice_match_length;

        while in_next < in_end && !os.overflow {
            stop.check()?;

            let in_block_begin = in_next;
            let in_max_block_end =
                choose_max_block_end(in_next, in_end, FAST_SOFT_MAX_BLOCK_LENGTH);
            let block_length = in_max_block_end - in_block_begin;

            // Save output state for rollback if static Huffman expands.
            // We do NOT save/restore in_base_offset — matchfinder entries
            // from a rolled-back block are naturally rejected by the cutoff check.
            let saved_pos = os.pos;
            let saved_bitbuf = os.bitbuf;
            let saved_bitcount = os.bitcount;

            // Emit block header: BFINAL + BTYPE=01 (static Huffman)
            let is_final = !self.force_nonfinal && in_max_block_end >= in_end;
            os.add_bits(is_final as u32, 1);
            os.add_bits(DEFLATE_BLOCKTYPE_STATIC_HUFFMAN, 2);
            os.flush_bits();

            // Pull bitbuf/bitcount into locals for the hot loop
            let mut bitbuf = os.bitbuf;
            let mut bitcount = os.bitcount;

            macro_rules! add_bits {
                ($bits:expr, $n:expr) => {{
                    bitbuf |= ($bits as u64) << bitcount;
                    bitcount += $n;
                }};
            }

            macro_rules! flush_bits {
                () => {{
                    if os.pos + 8 <= os.buf.len() {
                        crate::fast_bytes::store_u64_le(os.buf, os.pos, bitbuf);
                        os.pos += (bitcount >> 3) as usize;
                        bitbuf >>= bitcount & !7;
                        bitcount &= 7;
                    } else {
                        while bitcount >= 8 {
                            if os.pos < os.buf.len() {
                                os.buf[os.pos] = bitbuf as u8;
                                os.pos += 1;
                                bitcount -= 8;
                                bitbuf >>= 8;
                            } else {
                                os.overflow = true;
                                break;
                            }
                        }
                    }
                }};
            }

            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(load_u32_le(input, in_next), TURBO_MF_HASH_ORDER)
            } else {
                0
            };

            while in_next < in_max_block_end && !os.overflow {
                let remaining = in_end - in_next;
                let max_len = remaining.min(DEFLATE_MAX_MATCH_LEN as usize) as u32;
                let nice = max_len.min(nice_len);

                if max_len >= TURBO_REQUIRED_NBYTES {
                    let (length, offset) = mf.longest_match(
                        input,
                        &mut in_base_offset,
                        in_next,
                        max_len,
                        nice,
                        &mut next_hash,
                    );

                    if length > 0 {
                        let offset_slot = get_offset_slot(offset) as usize;
                        add_bits!(
                            full_len_cw[length as usize],
                            full_len_bits[length as usize] as u32
                        );
                        add_bits!(
                            sc.codewords_offset[offset_slot],
                            sc.lens_offset[offset_slot] as u32
                        );
                        add_bits!(
                            offset - DEFLATE_OFFSET_BASE[offset_slot],
                            DEFLATE_OFFSET_EXTRA_BITS[offset_slot] as u32
                        );
                        flush_bits!();

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

                // Emit literal inline
                let lit = input[in_next] as usize;
                add_bits!(sc.codewords_litlen[lit], sc.lens_litlen[lit] as u32);
                flush_bits!();
                in_next += 1;
            }

            // Emit end-of-block symbol (static: 7 bits)
            add_bits!(
                sc.codewords_litlen[DEFLATE_END_OF_BLOCK as usize],
                sc.lens_litlen[DEFLATE_END_OF_BLOCK as usize] as u32
            );
            flush_bits!();

            // Sync locals back
            os.bitbuf = bitbuf;
            os.bitcount = bitcount;

            // Check if static Huffman expanded the block. If so, rollback
            // the output and emit an uncompressed block instead.
            let static_bytes = os.pos.saturating_sub(saved_pos);
            // Uncompressed cost: data + 5 bytes per 64K sub-block + 1 alignment byte
            let uncomp_bytes = block_length + 5 * block_length.div_ceil(0xFFFF) + 1;
            if os.overflow || static_bytes > uncomp_bytes {
                os.pos = saved_pos;
                os.bitbuf = saved_bitbuf;
                os.bitcount = saved_bitcount;
                os.overflow = false;
                // Advance in_next to block end. The matchfinder only saw
                // positions up to in_next; those entries get naturally cut off
                // by the distance check on future blocks.
                in_next = in_max_block_end;
                let is_final_actual = !self.force_nonfinal && in_next >= in_end;
                Self::write_uncompressed(
                    os,
                    &input[in_block_begin..in_max_block_end],
                    is_final_actual,
                );
            }
        }

        self.turbo_mf = Some(mf);
        Ok(())
    }

    /// Write uncompressed DEFLATE block(s), splitting at 64KB boundaries.
    fn write_uncompressed(os: &mut OutputBitstream<'_>, data: &[u8], is_final_block: bool) {
        let mut remaining = data;
        while !remaining.is_empty() {
            let is_last = remaining.len() <= 0xFFFF;
            let len = remaining.len().min(0xFFFF);
            let chunk = &remaining[..len];
            remaining = &remaining[len..];

            let bfinal = if is_last && is_final_block { 1u8 } else { 0 };

            // BFINAL + BTYPE (uncompressed = 0), then align to byte boundary
            let byte = (bfinal << os.bitcount) | os.bitbuf as u8;
            os.write_byte(byte);
            if os.bitcount > 5 {
                os.write_byte(0);
            }
            os.bitbuf = 0;
            os.bitcount = 0;

            // LEN and NLEN
            os.write_le16(len as u16);
            os.write_le16(!len as u16);

            // Data
            os.write_bytes(chunk);
        }
    }

    /// Turbo compression using single-entry hash table matchfinder.
    ///
    /// Same greedy algorithm as compress_fastest, but uses the turbo matchfinder
    /// with limited hash updates for higher throughput.
    fn compress_turbo(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.turbo_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;

        // Dictionary warm-up
        if self.chunk_start > 0 && in_next + 4 <= in_end {
            let mut warmup_hash = lz_hash(
                crate::fast_bytes::load_u32_le(input, 0),
                TURBO_MF_HASH_ORDER,
            );
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                self.chunk_start as u32,
                &mut warmup_hash,
            );
        }

        while in_next < in_end && !os.overflow {
            stop.check()?;
            let in_block_begin = in_next;
            let in_max_block_end =
                choose_max_block_end(in_next, in_end, FAST_SOFT_MAX_BLOCK_LENGTH);
            let mut seq_idx = 0;

            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(
                    crate::fast_bytes::load_u32_le(input, in_next),
                    TURBO_MF_HASH_ORDER,
                )
            } else {
                0
            };

            while in_next < in_max_block_end && seq_idx < FAST_SEQ_STORE_LENGTH {
                let remaining = in_end - in_next;
                let max_len = remaining.min(DEFLATE_MAX_MATCH_LEN as usize) as u32;
                let nice_len = max_len.min(self.nice_match_length);

                if max_len >= TURBO_REQUIRED_NBYTES {
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
            let is_final = !self.force_nonfinal && in_next >= in_end;
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

        self.turbo_mf = Some(mf);
        Ok(())
    }

    /// FastHt compression using 2-entry hash table with limited updates.
    ///
    /// Same greedy algorithm as compress_fastest, but uses the fast_ht matchfinder
    /// which has 2 entries per bucket (better match quality) and limited hash
    /// updates on skips (faster than full hash chains).
    fn compress_fast_ht(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.fast_ht_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;

        // Dictionary warm-up
        if self.chunk_start > 0 && in_next + 4 <= in_end {
            let mut warmup_hash = lz_hash(
                crate::fast_bytes::load_u32_le(input, 0),
                FAST_HT_MF_HASH_ORDER,
            );
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                self.chunk_start as u32,
                &mut warmup_hash,
            );
        }

        while in_next < in_end && !os.overflow {
            stop.check()?;
            let in_block_begin = in_next;
            let in_max_block_end =
                choose_max_block_end(in_next, in_end, FAST_SOFT_MAX_BLOCK_LENGTH);
            let mut seq_idx = 0;

            self.freqs.reset();
            self.sequences[0].litrunlen_and_length = 0;

            let mut next_hash = if in_next + 4 <= in_end {
                lz_hash(
                    crate::fast_bytes::load_u32_le(input, in_next),
                    FAST_HT_MF_HASH_ORDER,
                )
            } else {
                0
            };

            while in_next < in_max_block_end && seq_idx < FAST_SEQ_STORE_LENGTH {
                let remaining = in_end - in_next;
                let max_len = remaining.min(DEFLATE_MAX_MATCH_LEN as usize) as u32;
                let nice_len = max_len.min(self.nice_match_length);

                if max_len >= FAST_HT_REQUIRED_NBYTES {
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
            let is_final = !self.force_nonfinal && in_next >= in_end;
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

        self.fast_ht_mf = Some(mf);
        Ok(())
    }

    /// Greedy compression using hash chains matchfinder.
    ///
    /// Always takes the longest match at each position. Uses block splitting
    /// and adaptive min_match_len heuristic.
    fn compress_greedy(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.hc_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut next_hashes = [0u32; 2];
        let max_search_depth = self.max_search_depth;
        let good_match = self.good_match;

        // Dictionary warm-up: seed hash chains with positions before chunk_start
        if self.chunk_start > 0 && self.chunk_start + 5 <= in_end {
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                in_end,
                self.chunk_start as u32,
                &mut next_hashes,
            );
        }

        while in_next < in_end && !os.overflow {
            stop.check()?;
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
                    good_match,
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
            let is_final = !self.force_nonfinal && in_next >= in_end;
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
        Ok(())
    }

    /// Levels 5-9: lazy/lazy2 compression using hash chains matchfinder.
    ///
    /// Before committing to a match, looks ahead 1 position (lazy) or 2
    /// positions (lazy2) for a better match. Uses block splitting and
    /// adaptive min_match_len with periodic recalculation.
    fn compress_lazy_generic(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        lazy2: bool,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = self.hc_mf.take().unwrap();
        mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut next_hashes = [0u32; 2];
        let max_search_depth = self.max_search_depth;
        let good_match = self.good_match;
        let max_lazy = self.max_lazy;

        // Dictionary warm-up: seed hash chains with positions before chunk_start
        if self.chunk_start > 0 && self.chunk_start + 5 <= in_end {
            mf.skip_bytes(
                input,
                &mut in_base_offset,
                0,
                in_end,
                self.chunk_start as u32,
                &mut next_hashes,
            );
        }

        while in_next < in_end && !os.overflow {
            stop.check()?;
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
                    good_match,
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
                        if cur_len >= nice_len || cur_len >= max_lazy {
                            // Very long match — take it immediately, no lookahead
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
                            good_match,
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
                                good_match,
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
            let is_final = !self.force_nonfinal && in_next >= in_end;
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
        Ok(())
    }

    /// Levels 10-12: near-optimal compression using binary tree matchfinder.
    ///
    /// Finds all matches at each position, caches them, then uses iterative
    /// backward DP to find the minimum-cost literal/match path.
    fn compress_near_optimal(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut ns = self.near_optimal.take().unwrap();
        ns.bt_mf.init();

        let in_end = input.len();
        let mut in_next = self.chunk_start;
        let mut in_base_offset = 0usize;
        let mut max_len = DEFLATE_MAX_MATCH_LEN;
        let mut nice_len = max_len.min(self.nice_match_length);
        let mut cache_idx = 0usize;
        let mut next_hashes = [0u32; 2];
        let mut prev_block_used_only_literals = false;
        let max_search_depth = self.max_search_depth;
        let mut in_block_begin = self.chunk_start;
        #[cfg(feature = "unchecked")]
        let input_ptr = input.as_ptr();

        // Dictionary warm-up: seed binary tree with positions before chunk_start
        if self.chunk_start > 0 {
            for warm_pos in 0..self.chunk_start {
                let remaining = in_end - warm_pos;
                adjust_max_and_nice_len(&mut max_len, &mut nice_len, remaining);
                if max_len >= BT_MATCHFINDER_REQUIRED_NBYTES {
                    let cur_pos = warm_pos as i32; // in_base_offset is 0
                    #[cfg(feature = "unchecked")]
                    // SAFETY: warm_pos + nice_len <= in_end (guarded by adjust_max_and_nice_len)
                    unsafe {
                        ns.bt_mf.skip_byte_raw(
                            input_ptr,
                            in_base_offset,
                            cur_pos,
                            nice_len,
                            max_search_depth,
                            &mut next_hashes,
                        );
                    }
                    #[cfg(not(feature = "unchecked"))]
                    ns.bt_mf.skip_byte(
                        input,
                        in_base_offset,
                        cur_pos,
                        nice_len,
                        max_search_depth,
                        &mut next_hashes,
                    );
                }
            }
            // Reset max_len/nice_len for the actual compression
            max_len = DEFLATE_MAX_MATCH_LEN;
            nice_len = max_len.min(self.nice_match_length);
        }

        let mut in_next_slide = in_next + (in_end - in_next).min(MATCHFINDER_WINDOW_SIZE as usize);

        init_stats(&mut self.split_stats, &mut ns);

        loop {
            stop.check()?;
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
                    #[cfg(feature = "unchecked")]
                    // SAFETY: in_next + max_len <= in_end (guarded by adjust_max_and_nice_len).
                    // cache_idx + MAX_MATCHES_PER_POS < match_cache.len() (guarded by cache overflow check).
                    let num_matches = unsafe {
                        ns.bt_mf.get_matches_raw(
                            input_ptr,
                            in_base_offset,
                            cur_pos,
                            max_len,
                            nice_len,
                            max_search_depth,
                            &mut next_hashes,
                            ns.match_cache.as_mut_ptr().add(cache_idx),
                        )
                    };
                    #[cfg(not(feature = "unchecked"))]
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
                        #[cfg(feature = "unchecked")]
                        let lit = unsafe { *input_ptr.add(in_next) };
                        #[cfg(not(feature = "unchecked"))]
                        let lit = input[in_next];
                        self.split_stats.observe_literal(lit);
                        next_observation = in_next + 1;
                    }
                }

                // Write sentinel: num_matches and literal value
                let num_matches = cache_idx - matches_start;
                #[cfg(feature = "unchecked")]
                let lit_byte = unsafe { *input_ptr.add(in_next) };
                #[cfg(not(feature = "unchecked"))]
                let lit_byte = input[in_next];
                ns.match_cache[cache_idx] = LzMatch {
                    length: num_matches as u16,
                    offset: lit_byte as u16,
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
                            #[cfg(feature = "unchecked")]
                            // SAFETY: in_next + nice_len <= in_end
                            unsafe {
                                ns.bt_mf.skip_byte_raw(
                                    input_ptr,
                                    in_base_offset,
                                    cur_pos,
                                    nice_len,
                                    max_search_depth,
                                    &mut next_hashes,
                                );
                            }
                            #[cfg(not(feature = "unchecked"))]
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
                        #[cfg(feature = "unchecked")]
                        let skip_lit = unsafe { *input_ptr.add(in_next) };
                        #[cfg(not(feature = "unchecked"))]
                        let skip_lit = input[in_next];
                        ns.match_cache[cache_idx] = LzMatch {
                            length: 0,
                            offset: skip_lit as u16,
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
                    self.level.effort(),
                    self.level.is_libdeflate_compat(),
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
                let is_final = !self.force_nonfinal && in_next == in_end;

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
                    self.level.effort(),
                    self.level.is_libdeflate_compat(),
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
        Ok(())
    }

    /// Compress a chunk of input with optional dictionary prefix (for parallel compression).
    ///
    /// The input slice contains `[dict_bytes | chunk_bytes]` where dictionary
    /// bytes are `input[0..chunk_start]` and actual data is `input[chunk_start..]`.
    /// Only the data portion contributes to the DEFLATE output.
    ///
    /// If `is_last_chunk` is false, a sync flush (empty stored block) is appended
    /// to byte-align the output for concatenation with subsequent chunks.
    fn deflate_compress_chunk(
        &mut self,
        input: &[u8],
        chunk_start: usize,
        is_last_chunk: bool,
        output: &mut [u8],
        stop: &impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        // Store: no matchfinder, just uncompressed blocks of the data portion.
        if self.level.strategy() == InternalStrategy::Store {
            return deflate_compress_none_chunk(&input[chunk_start..], output, is_last_chunk);
        }

        self.chunk_start = chunk_start;
        self.force_nonfinal = !is_last_chunk;

        let mut os = OutputBitstream::new(output);

        let result = match self.level.strategy() {
            InternalStrategy::StaticTurbo => self.compress_static_turbo(&mut os, input, stop),
            InternalStrategy::Turbo => self.compress_turbo(&mut os, input, stop),
            InternalStrategy::FastHt => self.compress_fast_ht(&mut os, input, stop),
            InternalStrategy::HtGreedy => self.compress_fastest(&mut os, input, stop),
            InternalStrategy::Greedy => self.compress_greedy(&mut os, input, stop),
            InternalStrategy::Lazy => self.compress_lazy_generic(&mut os, input, false, stop),
            InternalStrategy::Lazy2 => self.compress_lazy_generic(&mut os, input, true, stop),
            InternalStrategy::NearOptimal => self.compress_near_optimal(&mut os, input, stop),
            InternalStrategy::Store => unreachable!(),
        };
        if let Err(e) = result {
            self.chunk_start = 0;
            self.force_nonfinal = false;
            return Err(e);
        }

        if os.overflow {
            self.chunk_start = 0;
            self.force_nonfinal = false;
            return Err(CompressionError::InsufficientSpace);
        }

        if !is_last_chunk {
            // Sync flush: empty stored block (BFINAL=0, BTYPE=00) for byte alignment.
            os.add_bits(0, 3); // BFINAL=0 + BTYPE=00
            os.flush_bits();
            if os.bitcount > 0 {
                if os.pos < os.buf.len() {
                    os.buf[os.pos] = os.bitbuf as u8;
                    os.pos += 1;
                } else {
                    self.chunk_start = 0;
                    self.force_nonfinal = false;
                    return Err(CompressionError::InsufficientSpace);
                }
            }
            // LEN=0, NLEN=0xFFFF
            os.write_le16(0x0000);
            os.write_le16(0xFFFF);
        } else {
            // Last chunk: write final partial byte.
            if os.bitcount > 0 {
                if os.pos < os.buf.len() {
                    os.buf[os.pos] = os.bitbuf as u8;
                    os.pos += 1;
                } else {
                    self.chunk_start = 0;
                    self.force_nonfinal = false;
                    return Err(CompressionError::InsufficientSpace);
                }
            }
        }

        if os.overflow {
            self.chunk_start = 0;
            self.force_nonfinal = false;
            return Err(CompressionError::InsufficientSpace);
        }

        self.chunk_start = 0;
        self.force_nonfinal = false;
        Ok(os.pos)
    }

    /// Simple literal-only compressor that exercises the full block flushing path.
    #[allow(dead_code)]
    fn compress_literals(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut pos = 0;

        while pos < input.len() && !os.overflow {
            stop.check()?;
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
        Ok(())
    }

    /// Compress data in gzip format using multiple threads.
    ///
    /// Splits the input into chunks (one per thread), each with a 32KB dictionary
    /// overlap from the previous chunk. All chunks are compressed in parallel, then
    /// concatenated into a valid gzip stream.
    ///
    /// The compression ratio is nearly identical to single-threaded compression,
    /// since each chunk uses a full 32KB dictionary window.
    ///
    /// Falls back to single-threaded compression for small inputs or `num_threads <= 1`.
    ///
    /// ```
    /// use zenflate::{Compressor, CompressionLevel, Decompressor, Unstoppable};
    ///
    /// let data = vec![0u8; 100_000];
    /// let mut compressor = Compressor::new(CompressionLevel::balanced());
    /// let bound = Compressor::gzip_compress_bound(data.len()) + 4 * 5;
    /// let mut compressed = vec![0u8; bound];
    /// let csize = compressor.gzip_compress_parallel(&data, &mut compressed, 4, Unstoppable).unwrap();
    ///
    /// let mut decompressor = Decompressor::new();
    /// let mut output = vec![0u8; data.len()];
    /// let result = decompressor.gzip_decompress(&compressed[..csize], &mut output, Unstoppable).unwrap();
    /// assert_eq!(&output[..result.output_written], &data[..]);
    /// ```
    #[cfg(feature = "std")]
    pub fn gzip_compress_parallel(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        num_threads: usize,
        stop: impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        use crate::checksum::crc32_combine;
        use alloc::vec;
        use alloc::vec::Vec;

        let num_threads = num_threads.max(1);
        let level = self.level;

        // For small inputs or single thread, fall back to single-threaded.
        if num_threads == 1 || input.len() < 32 * 1024 {
            return self.gzip_compress(input, output, stop);
        }

        const DICT_SIZE: usize = 32 * 1024; // MATCHFINDER_WINDOW_SIZE

        // Split into equal-sized chunks, one per thread. At least 16KB per chunk.
        let num_chunks = num_threads.min(input.len() / (16 * 1024)).max(1);
        let chunk_data_size = input.len().div_ceil(num_chunks);

        // Build chunk descriptors: (dict_start, data_start, data_end, is_last)
        let mut chunks: Vec<(usize, usize, usize, bool)> = Vec::with_capacity(num_chunks);
        for i in 0..num_chunks {
            let data_start = i * chunk_data_size;
            let data_end = ((i + 1) * chunk_data_size).min(input.len());
            if data_start >= input.len() {
                break;
            }
            let dict_start = data_start.saturating_sub(DICT_SIZE);
            let is_last = data_end >= input.len();
            chunks.push((dict_start, data_start, data_end, is_last));
        }

        // Parallel compression: each thread gets its own Compressor.
        // Each result is (compressed_bytes, crc32, data_len).
        #[allow(clippy::type_complexity)]
        let results: Vec<Result<(Vec<u8>, u32, usize), CompressionError>> =
            std::thread::scope(|s| {
                let handles: Vec<_> = chunks
                    .iter()
                    .map(|&(dict_start, data_start, data_end, is_last)| {
                        let stop = &stop;
                        s.spawn(move || {
                            let mut c = Compressor::new(level);
                            let chunk_input = &input[dict_start..data_end];
                            let chunk_start = data_start - dict_start;
                            let data_len = data_end - data_start;

                            // CRC-32 of the data portion only.
                            let chunk_crc = crc32(0, &input[data_start..data_end]);

                            // Compress chunk.
                            let bound = Compressor::deflate_compress_bound(chunk_input.len()) + 5;
                            let mut buf = vec![0u8; bound];
                            let size = c.deflate_compress_chunk(
                                chunk_input,
                                chunk_start,
                                is_last,
                                &mut buf,
                                stop,
                            )?;
                            buf.truncate(size);

                            Ok((buf, chunk_crc, data_len))
                        })
                    })
                    .collect();

                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

        // Assemble gzip output: 10-byte header + concatenated DEFLATE + 8-byte footer.
        let mut total_deflate_size = 0usize;
        let mut combined_crc = 0u32;
        for result in &results {
            match result {
                Ok((buf, chunk_crc, data_len)) => {
                    total_deflate_size += buf.len();
                    combined_crc = crc32_combine(combined_crc, *chunk_crc, *data_len);
                }
                Err(e) => return Err(*e),
            }
        }

        let total_size = 10 + total_deflate_size + 8;
        if total_size > output.len() {
            return Err(CompressionError::InsufficientSpace);
        }

        // gzip header (10 bytes).
        output[0] = 0x1F; // ID1
        output[1] = 0x8B; // ID2
        output[2] = 0x08; // CM = deflate
        output[3] = 0x00; // FLG = none
        output[4..8].copy_from_slice(&[0, 0, 0, 0]); // MTIME
        output[8] = 0x00; // XFL
        output[9] = 0xFF; // OS = unknown

        // Concatenate DEFLATE chunks.
        let mut pos = 10;
        for result in &results {
            let (buf, _, _) = result.as_ref().unwrap();
            output[pos..pos + buf.len()].copy_from_slice(buf);
            pos += buf.len();
        }

        // CRC-32 + ISIZE (8 bytes).
        output[pos..pos + 4].copy_from_slice(&combined_crc.to_le_bytes());
        pos += 4;
        output[pos..pos + 4].copy_from_slice(&(input.len() as u32).to_le_bytes());
        pos += 4;

        Ok(pos)
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

/// Level 0 chunk variant: output uncompressed blocks with BFINAL control.
fn deflate_compress_none_chunk(
    input: &[u8],
    output: &mut [u8],
    is_last: bool,
) -> Result<usize, CompressionError> {
    if input.is_empty() {
        if output.len() < 5 {
            return Err(CompressionError::InsufficientSpace);
        }
        let bfinal = if is_last { 1u8 } else { 0 };
        output[0] = bfinal | (DEFLATE_BLOCKTYPE_UNCOMPRESSED << 1) as u8;
        output[1..5].copy_from_slice(&[0, 0, 0xFF, 0xFF]);
        return Ok(5);
    }

    let mut in_pos = 0;
    let mut out_pos = 0;

    while in_pos < input.len() {
        let is_last_block = input.len() - in_pos <= 0xFFFF;
        let len = (input.len() - in_pos).min(0xFFFF);

        if out_pos + 5 + len > output.len() {
            return Err(CompressionError::InsufficientSpace);
        }

        let bfinal = if is_last && is_last_block { 1u8 } else { 0 };
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
        let mut c = Compressor::new(CompressionLevel::none());
        let mut output = vec![0u8; 100];
        let size = c
            .deflate_compress(&[], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(size, 5);

        // Decompress with our own decompressor
        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; 0];
        let dsize = d
            .deflate_decompress(&output[..size], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(dsize, 0);
    }

    #[test]
    fn test_compress_level0_roundtrip() {
        let data = b"Hello, World! This is a test of uncompressed DEFLATE blocks.";
        let mut c = Compressor::new(CompressionLevel::none());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_level0_large() {
        let data: Vec<u8> = (0..=255).cycle().take(200_000).collect();
        let mut c = Compressor::new(CompressionLevel::none());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_literals_roundtrip() {
        // Test the literal-only compressor at level 6 (no matchfinding yet)
        let data = b"Hello, World! This is a test of literal-only DEFLATE compression.";
        let mut c = Compressor::new(CompressionLevel::balanced());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_compress_literals_large() {
        let data: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        let mut c = Compressor::new(CompressionLevel::balanced());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

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
        let mut c = Compressor::new(CompressionLevel::balanced());
        let bound = Compressor::zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .zlib_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .zlib_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    fn test_compress_gzip_roundtrip() {
        let data = b"Test gzip compression roundtrip!";
        let mut c = Compressor::new(CompressionLevel::balanced());
        let bound = Compressor::gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .gzip_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .gzip_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_cross_decompress_libdeflater() {
        // Compress with zenflate, decompress with libdeflater
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        let mut c = Compressor::new(CompressionLevel::balanced());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

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
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap_or_else(|e| panic!("level {level}: compress failed: {e}"));

        // Verify with our own decompressor
        let mut d = crate::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = d
            .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap_or_else(|e| panic!("level {level}: zenflate decompress failed: {e}"))
            .output_written;
        assert_eq!(
            &decompressed[..dsize],
            data,
            "level {level}: zenflate roundtrip mismatch"
        );

        // Verify with libdeflater (skip under miri — can't run C FFI)
        #[cfg(not(miri))]
        {
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
        let mut c = Compressor::new(CompressionLevel::fastest());
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();
        // All zeros should compress very well
        assert!(
            csize < data.len() / 10,
            "Level 1 should compress all-zeros well: {csize} >= {}",
            data.len() / 10
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)]
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
            .deflate_decompress(
                &c_compressed[..c_csize],
                &mut decompressed,
                enough::Unstoppable,
            )
            .unwrap()
            .output_written;
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
        // Test all effort levels 0-30 with the same data
        let data: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        for effort in 0..=30 {
            roundtrip_verify(&data, effort);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_libdeflate_compat_roundtrip() {
        // Test all libdeflate() levels 0-12 roundtrip correctly
        let data: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::libdeflate(level));
            let bound = Compressor::deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .deflate_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("libdeflate({level}): compress failed: {e}"));

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("libdeflate({level}): decompress failed: {e}"))
                .output_written;
            assert_eq!(
                &decompressed[..dsize],
                &data[..],
                "libdeflate({level}): roundtrip mismatch"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)]
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
                .deflate_decompress(
                    &c_compressed[..c_csize],
                    &mut decompressed,
                    enough::Unstoppable,
                )
                .unwrap_or_else(|e| {
                    panic!("level {level}: zenflate decompress of C output failed: {e}")
                })
                .output_written;
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
            let csize = c
                .deflate_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap();
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
                .zlib_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("level {level}: zlib compress failed: {e}"));

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .zlib_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("level {level}: zlib decompress failed: {e}"))
                .output_written;
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
                .gzip_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("level {level}: gzip compress failed: {e}"));

            let mut d = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dsize = d
                .gzip_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
                .unwrap_or_else(|e| panic!("level {level}: gzip decompress failed: {e}"))
                .output_written;
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

    /// Verify parallel gzip compression produces valid output by decompressing
    /// and comparing to original input.
    fn parallel_roundtrip(data: &[u8], level: u32, num_threads: usize) {
        let level = CompressionLevel::new(level);
        let bound = Compressor::gzip_compress_bound(data.len()) + num_threads * 5;
        let mut compressed = vec![0u8; bound];
        let mut compressor = Compressor::new(level);
        let csize = compressor
            .gzip_compress_parallel(data, &mut compressed, num_threads, enough::Unstoppable)
            .unwrap();

        let mut decompressor = crate::decompress::Decompressor::new();
        let mut decompressed = vec![0u8; data.len()];
        let dsize = decompressor
            .gzip_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
            .unwrap()
            .output_written;
        assert_eq!(dsize, data.len(), "decompressed size mismatch");
        assert_eq!(&decompressed[..dsize], data, "data mismatch");
    }

    #[test]
    fn test_parallel_gzip_level1() {
        // 256KB of mixed data — enough for 4 chunks
        let data = make_mixed_data(256 * 1024);
        parallel_roundtrip(&data, 1, 4);
    }

    #[test]
    fn test_parallel_gzip_level6() {
        let data = make_mixed_data(256 * 1024);
        parallel_roundtrip(&data, 6, 4);
    }

    #[test]
    fn test_parallel_gzip_level12() {
        let data = make_mixed_data(256 * 1024);
        parallel_roundtrip(&data, 12, 4);
    }

    #[test]
    fn test_parallel_gzip_all_levels() {
        let data = make_mixed_data(128 * 1024);
        for level in 0..=12 {
            for threads in [1, 2, 4] {
                parallel_roundtrip(&data, level, threads);
            }
        }
    }

    #[test]
    fn test_parallel_gzip_zeros() {
        let data = vec![0u8; 256 * 1024];
        parallel_roundtrip(&data, 1, 4);
        parallel_roundtrip(&data, 6, 4);
        parallel_roundtrip(&data, 12, 4);
    }

    #[test]
    fn test_parallel_gzip_sequential() {
        let data: Vec<u8> = (0..256 * 1024).map(|i| (i % 256) as u8).collect();
        parallel_roundtrip(&data, 1, 4);
        parallel_roundtrip(&data, 6, 4);
        parallel_roundtrip(&data, 12, 4);
    }

    #[test]
    fn test_parallel_gzip_small_input() {
        // Small input should fall back to single-threaded
        let data = b"Hello, World!";
        parallel_roundtrip(data, 6, 4);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_parallel_gzip_matches_single_threaded_crc() {
        // Verify the CRC-32 in the parallel output is correct by having
        // libdeflater (C) decompress it.
        let data = make_mixed_data(256 * 1024);
        let level = CompressionLevel::new(6);
        let bound = Compressor::gzip_compress_bound(data.len()) + 4 * 5;
        let mut compressed = vec![0u8; bound];
        let mut compressor = Compressor::new(level);
        let csize = compressor
            .gzip_compress_parallel(&data, &mut compressed, 4, enough::Unstoppable)
            .unwrap();

        // Decompress with C library to validate the gzip stream.
        let mut decompressed = vec![0u8; data.len()];
        let dsize = libdeflater::Decompressor::new()
            .gzip_decompress(&compressed[..csize], &mut decompressed)
            .unwrap();
        assert_eq!(dsize, data.len());
        assert_eq!(&decompressed[..dsize], &data[..]);
    }

    /// Generate mixed data that's representative of real workloads.
    fn make_mixed_data(len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        // Mix of patterns: sequential, repeated, random-ish
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match i % 1024 {
                0..=255 => (i % 256) as u8,                    // sequential
                256..=511 => (i / 256 % 256) as u8,            // slow-changing
                512..=767 => b"Hello, World! "[i % 14],        // repeated text
                _ => (i.wrapping_mul(2654435761) >> 16) as u8, // pseudo-random
            };
        }
        data
    }

    // ---- Bug reproducer: greedy compressor corruption on adaptive-filtered PNG data ----

    /// PNG filter types (same as PNG spec).
    #[cfg(not(miri))]
    const FILTER_NONE: u8 = 0;
    #[cfg(not(miri))]
    const FILTER_SUB: u8 = 1;
    #[cfg(not(miri))]
    const FILTER_UP: u8 = 2;
    #[cfg(not(miri))]
    const FILTER_AVERAGE: u8 = 3;
    #[cfg(not(miri))]
    const FILTER_PAETH: u8 = 4;

    #[cfg(not(miri))]
    fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
        let a = a as i16;
        let b = b as i16;
        let c = c as i16;
        let p = a + b - c;
        let pa = (p - a).unsigned_abs();
        let pb = (p - b).unsigned_abs();
        let pc = (p - c).unsigned_abs();
        if pa <= pb && pa <= pc {
            a as u8
        } else if pb <= pc {
            b as u8
        } else {
            c as u8
        }
    }

    /// Apply a PNG filter to a row. Output written to `out`.
    #[cfg(not(miri))]
    fn apply_png_filter(filter: u8, row: &[u8], prev_row: &[u8], bpp: usize, out: &mut [u8]) {
        let len = row.len();
        match filter {
            FILTER_NONE => out[..len].copy_from_slice(row),
            FILTER_SUB => {
                let b = bpp.min(len);
                out[..b].copy_from_slice(&row[..b]);
                for i in bpp..len {
                    out[i] = row[i].wrapping_sub(row[i - bpp]);
                }
            }
            FILTER_UP => {
                for i in 0..len {
                    out[i] = row[i].wrapping_sub(prev_row[i]);
                }
            }
            FILTER_AVERAGE => {
                for i in 0..bpp.min(len) {
                    out[i] = row[i].wrapping_sub(prev_row[i] >> 1);
                }
                for i in bpp..len {
                    let avg = ((row[i - bpp] as u16 + prev_row[i] as u16) >> 1) as u8;
                    out[i] = row[i].wrapping_sub(avg);
                }
            }
            FILTER_PAETH => {
                for i in 0..bpp.min(len) {
                    out[i] = row[i].wrapping_sub(paeth_predictor(0, prev_row[i], 0));
                }
                for i in bpp..len {
                    let pred = paeth_predictor(row[i - bpp], prev_row[i], prev_row[i - bpp]);
                    out[i] = row[i].wrapping_sub(pred);
                }
            }
            _ => out[..len].copy_from_slice(row),
        }
    }

    /// MinSum (sum of absolute values) heuristic score for a filtered row.
    #[cfg(not(miri))]
    fn sav_score(data: &[u8]) -> u64 {
        data.iter()
            .map(|&b| if b > 128 { 256 - b as u64 } else { b as u64 })
            .sum()
    }

    /// Apply adaptive MinSum filtering to raw image data, producing PNG-style
    /// filtered output (filter byte + filtered row per scanline).
    #[cfg(not(miri))]
    fn filter_image_minsum(pixels: &[u8], row_bytes: usize, height: usize, bpp: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(height * (1 + row_bytes));
        let mut prev_row = vec![0u8; row_bytes];
        let mut candidates: Vec<Vec<u8>> = (0..5).map(|_| vec![0u8; row_bytes]).collect();

        for y in 0..height {
            let row = &pixels[y * row_bytes..(y + 1) * row_bytes];

            // Try all 5 filters, pick the one with lowest SAV score
            for f in 0..5u8 {
                apply_png_filter(f, row, &prev_row, bpp, &mut candidates[f as usize]);
            }

            let mut best_f = 0u8;
            let mut best_score = u64::MAX;
            for f in 0..5u8 {
                let score = sav_score(&candidates[f as usize]);
                if score < best_score {
                    best_score = score;
                    best_f = f;
                }
            }

            out.push(best_f);
            out.extend_from_slice(&candidates[best_f as usize]);

            prev_row.copy_from_slice(row);
        }

        out
    }

    /// Try to compress and decompress data at the given level. Returns Ok(())
    /// if roundtrip succeeds, Err(msg) if compression panics, errors, or
    /// produces corrupt output.
    #[cfg(not(miri))]
    fn try_roundtrip(data: &[u8], level: u32) -> Result<(), String> {
        let data_owned = data.to_vec();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut compressor = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::deflate_compress_bound(data_owned.len());
            let mut compressed = vec![0u8; bound];
            let csize = compressor
                .deflate_compress(&data_owned, &mut compressed, enough::Unstoppable)
                .map_err(|e| format!("compression error: {e}"))?;

            // Decompress with zenflate
            let mut decompressor = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data_owned.len()];
            let dsize = decompressor
                .deflate_decompress(&compressed[..csize], &mut decompressed, enough::Unstoppable)
                .map_err(|e| format!("decompression error: {e}"))?
                .output_written;

            if decompressed[..dsize] != data_owned[..] {
                return Err("roundtrip data mismatch".to_string());
            }

            // Cross-check with libdeflater
            let mut ld = libdeflater::Decompressor::new();
            let mut ld_out = vec![0u8; data_owned.len()];
            let ld_size = ld
                .deflate_decompress(&compressed[..csize], &mut ld_out)
                .map_err(|e| format!("libdeflater decompression error: {e}"))?;
            if ld_out[..ld_size] != data_owned[..] {
                return Err("libdeflater roundtrip mismatch".to_string());
            }

            Ok(())
        }));

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => Err(msg),
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    (*s).to_string()
                } else {
                    "unknown panic".to_string()
                };
                Err(format!("panic: {msg}"))
            }
        }
    }

    /// Bug reproducer: compressor produces corrupt deflate output (or panics
    /// with a bitstream overflow assertion) on adaptive MinSum-filtered PNG data
    /// from a 1024x1024 RGB8 image.
    ///
    /// The bug manifests as a `debug_assert!` failure in `add_bits()` during
    /// Regression test: adaptive PNG filtering can produce data patterns where
    /// the dynamic Huffman block header uses all 19 precode symbols, pushing
    /// the precode length output to 19×3=57 bits. With ≤7 residual bits after
    /// a flush, that's 64 total — exceeding the 63-bit bitbuffer capacity.
    ///
    /// Fixed by merging the first precode length with the header before
    /// flushing, matching libdeflate's approach. Remaining 18×3=54 bits
    /// safely fit: 7+54=61 ≤ 63.
    ///
    /// Source: codec-corpus/clic2025-1024/0d154749...f0.png
    #[test]
    #[ignore] // requires corpus file
    #[cfg(not(miri))]
    fn test_bitstream_overflow_adaptive_filtered_png() {
        let path = "/home/lilith/work/codec-corpus/clic2025-1024/\
                     0d154749c7771f58e89ad343653ec4e20d6f037da829f47f5598e5d0a4ab61f0.png";

        // Decode the PNG
        let file = std::fs::File::open(path)
            .unwrap_or_else(|e| panic!("failed to open corpus file {path}: {e}"));
        let decoder = png::Decoder::new(file);
        let mut reader = decoder.read_info().unwrap();
        let info = reader.info();
        let width = info.width as usize;
        let height = info.height as usize;
        assert_eq!(info.color_type, png::ColorType::Rgb, "expected RGB image");
        assert_eq!(info.bit_depth, png::BitDepth::Eight, "expected 8-bit depth");

        let bpp = 3; // RGB8
        let row_bytes = width * bpp;
        let mut pixels = vec![0u8; height * row_bytes];
        let frame_info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!(frame_info.width as usize, width);
        assert_eq!(frame_info.height as usize, height);

        // Apply adaptive MinSum filtering (same as zenpng's MinSum heuristic)
        let filtered = filter_image_minsum(&pixels, row_bytes, height, bpp);
        assert_eq!(filtered.len(), height * (1 + row_bytes));

        // All levels must roundtrip successfully
        for level in 1..=12 {
            try_roundtrip(&filtered, level).unwrap_or_else(|msg| panic!("L{level} failed: {msg}"));
        }
    }

    #[test]
    fn test_snapshot_restore_roundtrip() {
        // Compress two rows incrementally, then snapshot/restore/compress
        // and verify that restoring produces the same result as a fresh fork.
        let row1 = b"Hello, world! This is row one with some repetitive content content content.";
        let row2a = b"Row two variant A has different data patterns AAAA BBBB CCCC.";
        let row2b = b"Row two variant B has other patterns XXXX YYYY ZZZZ patterns.";

        let mut c = Compressor::new(CompressionLevel::new(15)); // Lazy
        let mut buf = Vec::with_capacity(row1.len() + row2a.len().max(row2b.len()));
        buf.extend_from_slice(row1);

        let mut out = vec![0u8; 4096];
        let size1 = c
            .deflate_compress_incremental(&buf, &mut out, false, enough::Unstoppable)
            .unwrap();
        assert!(size1 > 0);

        // Snapshot after row 1
        let snap = c.snapshot();

        // Try variant A
        buf.extend_from_slice(row2a);
        let mut out_a = vec![0u8; 4096];
        let size_a = c
            .deflate_compress_incremental(&buf, &mut out_a, true, enough::Unstoppable)
            .unwrap();

        // Restore and try variant B
        c.restore(snap);
        buf.truncate(row1.len());
        buf.extend_from_slice(row2b);
        let mut out_b = vec![0u8; 4096];
        let size_b = c
            .deflate_compress_incremental(&buf, &mut out_b, true, enough::Unstoppable)
            .unwrap();

        // Both should produce valid output (different sizes expected)
        assert!(size_a > 0);
        assert!(size_b > 0);
        // The variants should generally produce different compressed sizes
        // (they have different data), but we just check both are valid.
    }

    #[test]
    fn test_snapshot_restore_ht_strategy() {
        // Same test but with HtGreedy strategy (effort 10 maps to Greedy, use libdeflate L1)
        let row = b"Repetitive data for hash table matching matching matching matching.";

        let mut c = Compressor::new(CompressionLevel::libdeflate(1));
        let mut buf = Vec::from(&row[..]);
        let mut out = vec![0u8; 4096];

        let _size = c
            .deflate_compress_incremental(&buf, &mut out, false, enough::Unstoppable)
            .unwrap();
        let snap = c.snapshot();

        // Try extending with more data
        buf.extend_from_slice(b"Extra data appended.");
        let mut out2 = vec![0u8; 4096];
        let size2 = c
            .deflate_compress_incremental(&buf, &mut out2, true, enough::Unstoppable)
            .unwrap();
        assert!(size2 > 0);

        // Restore and try again with different data
        c.restore(snap);
        buf.truncate(row.len());
        buf.extend_from_slice(b"Different extension.");
        let mut out3 = vec![0u8; 4096];
        let size3 = c
            .deflate_compress_incremental(&buf, &mut out3, true, enough::Unstoppable)
            .unwrap();
        assert!(size3 > 0);
    }

    #[test]
    fn test_estimate_cost_incremental_basic() {
        let data = b"The quick brown fox jumps over the lazy dog. The quick brown fox jumps again.";
        let mut c = Compressor::new(CompressionLevel::new(15)); // Lazy

        let cost = c
            .deflate_estimate_cost_incremental(data, enough::Unstoppable)
            .unwrap();
        // Cost should be positive and reasonable (less than 8 bits per byte)
        assert!(cost > 0, "cost should be positive");
        assert!(
            cost < data.len() as u64 * 9,
            "cost {cost} should be less than {}, 9 bits/byte",
            data.len() as u64 * 9
        );
    }

    #[test]
    fn test_estimate_cost_ranking() {
        // Highly compressible data should have lower cost than random-like data
        let compressible = vec![b'A'; 200];
        let mixed: Vec<u8> = (0..200u8).collect();

        let mut c1 = Compressor::new(CompressionLevel::new(15));
        let cost_comp = c1
            .deflate_estimate_cost_incremental(&compressible, enough::Unstoppable)
            .unwrap();

        let mut c2 = Compressor::new(CompressionLevel::new(15));
        let cost_mixed = c2
            .deflate_estimate_cost_incremental(&mixed, enough::Unstoppable)
            .unwrap();

        assert!(
            cost_comp < cost_mixed,
            "compressible cost ({cost_comp}) should be less than mixed cost ({cost_mixed})"
        );
    }

    #[test]
    fn test_estimate_cost_with_snapshot() {
        // Verify that snapshot/restore works correctly with cost estimation
        let row1 = b"Initial row of data for context building.";
        let row2a = vec![b'X'; 50]; // compressible
        let row2b: Vec<u8> = (0..50).collect(); // less compressible

        let mut c = Compressor::new(CompressionLevel::new(15));
        let mut buf = Vec::from(&row1[..]);

        // Build context
        let _cost1 = c
            .deflate_estimate_cost_incremental(&buf, enough::Unstoppable)
            .unwrap();

        let snap = c.snapshot();

        // Estimate cost of variant A
        buf.extend_from_slice(&row2a);
        let cost_a = c
            .deflate_estimate_cost_incremental(&buf, enough::Unstoppable)
            .unwrap();

        // Restore and estimate cost of variant B
        c.restore(snap);
        buf.truncate(row1.len());
        buf.extend_from_slice(&row2b);
        let cost_b = c
            .deflate_estimate_cost_incremental(&buf, enough::Unstoppable)
            .unwrap();

        // Both costs should be positive
        assert!(cost_a > 0);
        assert!(cost_b > 0);
        // Repetitive 'X' data should be cheaper than sequential bytes
        assert!(
            cost_a < cost_b,
            "repetitive cost ({cost_a}) should be less than sequential cost ({cost_b})"
        );
    }

    #[test]
    fn test_estimate_cost_greedy_strategy() {
        let data = b"Greedy strategy test data with some repetition repetition repetition.";
        let mut c = Compressor::new(CompressionLevel::new(10)); // Greedy

        let cost = c
            .deflate_estimate_cost_incremental(data, enough::Unstoppable)
            .unwrap();
        assert!(cost > 0);
        assert!(cost < data.len() as u64 * 9);
    }

    #[test]
    fn test_estimate_cost_ht_strategy() {
        let data = b"HtGreedy strategy test data with repetition repetition repetition.";
        let mut c = Compressor::new(CompressionLevel::libdeflate(1)); // HtGreedy

        let cost = c
            .deflate_estimate_cost_incremental(data, enough::Unstoppable)
            .unwrap();
        assert!(cost > 0);
        assert!(cost < data.len() as u64 * 9);
    }

    #[test]
    fn test_estimate_cost_unsupported_strategy() {
        // NearOptimal strategy should return an error
        let data = b"test";
        let mut c = Compressor::new(CompressionLevel::new(25)); // NearOptimal

        let result = c.deflate_estimate_cost_incremental(data, enough::Unstoppable);
        assert!(result.is_err());
    }

    #[test]
    fn test_snapshot_clone() {
        let mut c = Compressor::new(CompressionLevel::new(15));
        let data = b"Some data to build state.";
        let mut buf = Vec::from(&data[..]);
        let mut out = vec![0u8; 4096];
        let _ = c
            .deflate_compress_incremental(&buf, &mut out, false, enough::Unstoppable)
            .unwrap();

        let snap = c.snapshot();
        let snap2 = snap.clone();

        // Both snapshots should restore to the same state
        buf.extend_from_slice(b"More data here.");

        c.restore(snap);
        let mut out1 = vec![0u8; 4096];
        let size1 = c
            .deflate_compress_incremental(&buf, &mut out1, true, enough::Unstoppable)
            .unwrap();

        c.restore(snap2);
        let mut out2 = vec![0u8; 4096];
        let size2 = c
            .deflate_compress_incremental(&buf, &mut out2, true, enough::Unstoppable)
            .unwrap();

        assert_eq!(size1, size2);
        assert_eq!(&out1[..size1], &out2[..size2]);
    }
}

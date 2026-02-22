//! Near-optimal DEFLATE compression (levels 10-12).
//!
//! Ported from libdeflate's near-optimal parsing code in `deflate_compress.c`.
//!
//! Uses a binary tree matchfinder to find all matches at each position,
//! caches them, then runs iterative backward DP to find the minimum-cost
//! literal/match path. Multiple optimization passes refine the cost model
//! from the resulting Huffman codes.

use crate::constants::*;
use crate::matchfinder::bt::{BtMatchfinder, LzMatch};

use super::bitstream::OutputBitstream;
use super::block::{
    BlockOutput, DeflateCodes, DeflateFreqs, EXTRA_PRECODE_BITS, LENGTH_SLOT,
    compute_precode_items, flush_block, make_huffman_codes,
};
use super::block_split::{BlockSplitStats, MIN_BLOCK_LENGTH, NUM_OBSERVATION_TYPES};
use super::huffman::make_huffman_code;
use super::sequences::Sequence;
use super::{SOFT_MAX_BLOCK_LENGTH, choose_min_match_len};

// ---- Constants ----

/// Fixed-point scaling for costs (16 = 4 fractional bits).
const BIT_COST: u32 = 16;

/// Cost assigned to symbols with no statistics (literal).
const LITERAL_NOSTAT_BITS: u32 = 13;

/// Cost assigned to symbols with no statistics (length).
const LENGTH_NOSTAT_BITS: u32 = 13;

/// Cost assigned to symbols with no statistics (offset).
const OFFSET_NOSTAT_BITS: u32 = 10;

/// Maximum match cache length.
pub(crate) const MATCH_CACHE_LENGTH: usize = SOFT_MAX_BLOCK_LENGTH * 5;

/// Maximum matches per position (one per possible length: 3..=258).
const MAX_MATCHES_PER_POS: usize =
    DEFLATE_MAX_MATCH_LEN as usize - DEFLATE_MIN_MATCH_LEN as usize + 1;

/// Maximum block length for near-optimal parsing.
const MAX_BLOCK_LENGTH: usize = {
    let a = SOFT_MAX_BLOCK_LENGTH + MIN_BLOCK_LENGTH - 1;
    let b = SOFT_MAX_BLOCK_LENGTH + 1 + DEFLATE_MAX_MATCH_LEN as usize;
    if a > b { a } else { b }
};

/// Bit shift for offset in OptimumNode.item.
pub(crate) const OPTIMUM_OFFSET_SHIFT: u32 = 9;

/// Mask for length in OptimumNode.item.
pub(crate) const OPTIMUM_LEN_MASK: u32 = (1 << OPTIMUM_OFFSET_SHIFT) - 1;

/// Total match cache allocation size.
const MATCH_CACHE_ALLOC_SIZE: usize =
    MATCH_CACHE_LENGTH + MAX_MATCHES_PER_POS + DEFLATE_MAX_MATCH_LEN as usize - 1;

/// Number of optimum nodes (one per position + sentinel).
const OPTIMUM_NODES_SIZE: usize = MAX_BLOCK_LENGTH + 1;

/// Size of the full offset-to-slot table.
const OFFSET_SLOT_FULL_SIZE: usize = DEFLATE_MAX_MATCH_OFFSET as usize + 1;

/// Size of the match length frequency tables.
const MATCH_LEN_FREQ_SIZE: usize = DEFLATE_MAX_MATCH_LEN as usize + 1;

// Storage types: fixed arrays when `unchecked` (single allocation via Box),
// Vec when safe (separate heap allocations per field).
#[cfg(feature = "unchecked")]
type MatchCacheTab = [LzMatch; MATCH_CACHE_ALLOC_SIZE];
#[cfg(not(feature = "unchecked"))]
type MatchCacheTab = Vec<LzMatch>;

#[cfg(feature = "unchecked")]
type OptimumNodesTab = [OptimumNode; OPTIMUM_NODES_SIZE];
#[cfg(not(feature = "unchecked"))]
type OptimumNodesTab = Vec<OptimumNode>;

#[cfg(feature = "unchecked")]
type OffsetSlotFullTab = [u8; OFFSET_SLOT_FULL_SIZE];
#[cfg(not(feature = "unchecked"))]
type OffsetSlotFullTab = Vec<u8>;

#[cfg(feature = "unchecked")]
type MatchLenFreqTab = [u32; MATCH_LEN_FREQ_SIZE];
#[cfg(not(feature = "unchecked"))]
type MatchLenFreqTab = Vec<u32>;

// ---- Data structures ----

/// A node in the optimal parsing graph.
///
/// For each position in the block, stores the cost to reach the end
/// and the best literal/match choice (encoded in `item`).
///
/// `item` encoding:
/// - Literal: `(literal_byte << OPTIMUM_OFFSET_SHIFT) | 1`
/// - Match: `(offset << OPTIMUM_OFFSET_SHIFT) | length`
#[derive(Clone, Copy, Default)]
pub(crate) struct OptimumNode {
    pub cost_to_end: u32,
    pub item: u32,
}

/// Cost model for the near-optimal parser.
///
/// Costs are in BIT_COST units (fixed-point with 4 fractional bits).
#[derive(Clone)]
pub(crate) struct DeflateCosts {
    /// Cost to output each possible literal (0..255).
    pub literal: [u32; DEFLATE_NUM_LITERALS as usize],
    /// Cost to output each possible match length (0..258, only 3..258 used).
    pub length: [u32; DEFLATE_MAX_MATCH_LEN as usize + 1],
    /// Cost to output a match offset of each possible offset slot (0..29).
    pub offset_slot: [u32; DEFLATE_NUM_OFFSET_SYMS as usize],
}

impl Default for DeflateCosts {
    fn default() -> Self {
        Self {
            literal: [0; DEFLATE_NUM_LITERALS as usize],
            length: [0; DEFLATE_MAX_MATCH_LEN as usize + 1],
            offset_slot: [0; DEFLATE_NUM_OFFSET_SYMS as usize],
        }
    }
}

/// All near-optimal-specific state, separate from the main Compressor.
///
/// With `unchecked`, all large fields are fixed arrays so `Box<NearOptimalState>`
/// is a single heap allocation (~9MB), matching libdeflate C's single malloc.
/// Without `unchecked`, fields use Vec (separate allocations per field).
///
/// Clone is available for the forking compressor use case (fork-per-row BF).
/// For L10-12 with `unchecked`, cloning puts ~9MB on the stack temporarily;
/// this is acceptable for the forking BF prototype which primarily targets L1-L4.
#[derive(Clone)]
pub(crate) struct NearOptimalState {
    /// Binary tree matchfinder.
    pub bt_mf: BtMatchfinder,
    /// Cached matches for the current block.
    pub match_cache: MatchCacheTab,
    /// Optimum nodes for the DP graph.
    pub optimum_nodes: OptimumNodesTab,
    /// Current cost model.
    pub costs: DeflateCosts,
    /// Saved cost model (from the best optimization pass).
    pub costs_saved: DeflateCosts,
    /// Full offset-to-slot mapping table.
    pub offset_slot_full: OffsetSlotFullTab,
    /// Literal/match statistics saved from previous block.
    pub prev_observations: [u32; NUM_OBSERVATION_TYPES],
    /// Total observations from previous block.
    pub prev_num_observations: u32,
    /// Approximate match length frequencies (new, not yet merged).
    pub new_match_len_freqs: MatchLenFreqTab,
    /// Approximate match length frequencies (merged).
    pub match_len_freqs: MatchLenFreqTab,
    /// Maximum optimization passes per block.
    pub max_optim_passes: u32,
    /// Minimum cost improvement to continue optimizing.
    pub min_improvement_to_continue: u32,
    /// Minimum savings to use a non-final pass's path.
    pub min_bits_to_use_nonfinal_path: u32,
    /// Maximum block length to consider static Huffman optimization.
    pub max_len_to_optimize_static_block: u32,
}

impl NearOptimalState {
    /// Create a new NearOptimalState with the given parameters.
    ///
    /// Returns `Box<Self>` to avoid stack overflow (struct is ~9MB).
    /// With `unchecked`, this is a single heap allocation matching C's pattern.
    /// Without `unchecked`, each Vec field is a separate allocation.
    #[cfg(not(feature = "unchecked"))]
    pub fn new(
        max_optim_passes: u32,
        min_improvement_to_continue: u32,
        min_bits_to_use_nonfinal_path: u32,
        max_len_to_optimize_static_block: u32,
    ) -> Box<Self> {
        let mut s = Box::new(Self {
            bt_mf: BtMatchfinder::new(),
            match_cache: alloc::vec![LzMatch::default(); MATCH_CACHE_ALLOC_SIZE],
            optimum_nodes: alloc::vec![OptimumNode::default(); OPTIMUM_NODES_SIZE],
            costs: DeflateCosts::default(),
            costs_saved: DeflateCosts::default(),
            offset_slot_full: alloc::vec![0u8; OFFSET_SLOT_FULL_SIZE],
            prev_observations: [0; NUM_OBSERVATION_TYPES],
            prev_num_observations: 0,
            new_match_len_freqs: alloc::vec![0u32; MATCH_LEN_FREQ_SIZE],
            match_len_freqs: alloc::vec![0u32; MATCH_LEN_FREQ_SIZE],
            max_optim_passes,
            min_improvement_to_continue,
            min_bits_to_use_nonfinal_path,
            max_len_to_optimize_static_block,
        });
        init_offset_slot_full(&mut s.offset_slot_full);
        s
    }

    /// Create a new NearOptimalState as a single heap allocation.
    ///
    /// Uses `Box::new_uninit()` to allocate ~9MB on the heap, then
    /// initializes each field through pointers (no stack copies).
    #[cfg(feature = "unchecked")]
    pub fn new(
        max_optim_passes: u32,
        min_improvement_to_continue: u32,
        min_bits_to_use_nonfinal_path: u32,
        max_len_to_optimize_static_block: u32,
    ) -> Box<Self> {
        use core::ptr;

        let mut boxed = Box::<Self>::new_uninit();
        let p = boxed.as_mut_ptr();

        // SAFETY: `p` points to a fresh heap allocation of size_of::<Self>().
        // We initialize every field before calling assume_init().
        unsafe {
            // BtMatchfinder tables
            BtMatchfinder::init_at(ptr::addr_of_mut!((*p).bt_mf));

            // match_cache — zero-init (LzMatch is two u16 zeros)
            let mc = ptr::addr_of_mut!((*p).match_cache) as *mut u8;
            core::ptr::write_bytes(mc, 0, core::mem::size_of::<MatchCacheTab>());

            // optimum_nodes — zero-init (OptimumNode is two u32 zeros)
            let on = ptr::addr_of_mut!((*p).optimum_nodes) as *mut u8;
            core::ptr::write_bytes(on, 0, core::mem::size_of::<OptimumNodesTab>());

            // offset_slot_full — init via helper
            let osf = ptr::addr_of_mut!((*p).offset_slot_full) as *mut u8;
            let osf_slice = core::slice::from_raw_parts_mut(osf, OFFSET_SLOT_FULL_SIZE);
            osf_slice.fill(0);
            init_offset_slot_full(osf_slice);

            // costs + costs_saved
            ptr::addr_of_mut!((*p).costs).write(DeflateCosts::default());
            ptr::addr_of_mut!((*p).costs_saved).write(DeflateCosts::default());

            // Scalar fields
            ptr::addr_of_mut!((*p).prev_observations).write([0; NUM_OBSERVATION_TYPES]);
            ptr::addr_of_mut!((*p).prev_num_observations).write(0);

            // Frequency tables — zero-init
            let nmlf = ptr::addr_of_mut!((*p).new_match_len_freqs) as *mut u8;
            core::ptr::write_bytes(nmlf, 0, core::mem::size_of::<MatchLenFreqTab>());
            let mlf = ptr::addr_of_mut!((*p).match_len_freqs) as *mut u8;
            core::ptr::write_bytes(mlf, 0, core::mem::size_of::<MatchLenFreqTab>());

            // Config scalars
            ptr::addr_of_mut!((*p).max_optim_passes).write(max_optim_passes);
            ptr::addr_of_mut!((*p).min_improvement_to_continue).write(min_improvement_to_continue);
            ptr::addr_of_mut!((*p).min_bits_to_use_nonfinal_path)
                .write(min_bits_to_use_nonfinal_path);
            ptr::addr_of_mut!((*p).max_len_to_optimize_static_block)
                .write(max_len_to_optimize_static_block);

            boxed.assume_init()
        }
    }
}

// ---- Offset slot full table ----

/// Build the full offset-to-slot mapping table.
fn init_offset_slot_full(table: &mut [u8]) {
    for slot in 0..30usize {
        let base = DEFLATE_OFFSET_BASE[slot] as usize;
        let count = 1usize << DEFLATE_OFFSET_EXTRA_BITS[slot];
        for j in 0..count {
            if base + j < table.len() {
                table[base + j] = slot as u8;
            }
        }
    }
}

// ---- Cost model functions ----

/// Default litlen cost lookup table.
///
/// Three entries for different match probability estimates:
/// - Index 0: match_prob ~0.25 (few matches)
/// - Index 1: match_prob ~0.5 (neutral)
/// - Index 2: match_prob ~0.75 (many matches)
///
/// Each entry maps num_used_literals (0..=256) to a literal cost and
/// provides a length symbol cost, both in BIT_COST units.
struct DefaultLitlenCosts {
    used_lits_to_lit_cost: [u8; 257],
    len_sym_cost: u8,
}

#[rustfmt::skip]
static DEFAULT_LITLEN_COSTS: [DefaultLitlenCosts; 3] = [
    // match_prob = 0.25 (few matches)
    DefaultLitlenCosts {
        used_lits_to_lit_cost: [
            6, 6, 22, 32, 38, 43, 48, 51, 54, 57, 59, 61, 64, 65, 67, 69,
            70, 72, 73, 74, 75, 76, 77, 79, 80, 80, 81, 82, 83, 84, 85, 85,
            86, 87, 88, 88, 89, 89, 90, 91, 91, 92, 92, 93, 93, 94, 95, 95,
            96, 96, 96, 97, 97, 98, 98, 99, 99, 99, 100, 100, 101, 101, 101, 102,
            102, 102, 103, 103, 104, 104, 104, 105, 105, 105, 105, 106, 106, 106, 107, 107,
            107, 108, 108, 108, 108, 109, 109, 109, 109, 110, 110, 110, 111, 111, 111, 111,
            112, 112, 112, 112, 112, 113, 113, 113, 113, 114, 114, 114, 114, 114, 115, 115,
            115, 115, 115, 116, 116, 116, 116, 116, 117, 117, 117, 117, 117, 118, 118, 118,
            118, 118, 118, 119, 119, 119, 119, 119, 120, 120, 120, 120, 120, 120, 121, 121,
            121, 121, 121, 121, 121, 122, 122, 122, 122, 122, 122, 123, 123, 123, 123, 123,
            123, 123, 124, 124, 124, 124, 124, 124, 124, 125, 125, 125, 125, 125, 125, 125,
            125, 126, 126, 126, 126, 126, 126, 126, 127, 127, 127, 127, 127, 127, 127, 127,
            128, 128, 128, 128, 128, 128, 128, 128, 128, 129, 129, 129, 129, 129, 129, 129,
            129, 129, 130, 130, 130, 130, 130, 130, 130, 130, 130, 131, 131, 131, 131, 131,
            131, 131, 131, 131, 131, 132, 132, 132, 132, 132, 132, 132, 132, 132, 132, 133,
            133, 133, 133, 133, 133, 133, 133, 133, 133, 134, 134, 134, 134, 134, 134, 134,
            134,
        ],
        len_sym_cost: 109,
    },
    // match_prob = 0.5 (neutral)
    DefaultLitlenCosts {
        used_lits_to_lit_cost: [
            16, 16, 32, 41, 48, 53, 57, 60, 64, 66, 69, 71, 73, 75, 76, 78,
            80, 81, 82, 83, 85, 86, 87, 88, 89, 90, 91, 92, 92, 93, 94, 95,
            96, 96, 97, 98, 98, 99, 99, 100, 101, 101, 102, 102, 103, 103, 104, 104,
            105, 105, 106, 106, 107, 107, 108, 108, 108, 109, 109, 110, 110, 110, 111, 111,
            112, 112, 112, 113, 113, 113, 114, 114, 114, 115, 115, 115, 115, 116, 116, 116,
            117, 117, 117, 118, 118, 118, 118, 119, 119, 119, 119, 120, 120, 120, 120, 121,
            121, 121, 121, 122, 122, 122, 122, 122, 123, 123, 123, 123, 124, 124, 124, 124,
            124, 125, 125, 125, 125, 125, 126, 126, 126, 126, 126, 127, 127, 127, 127, 127,
            128, 128, 128, 128, 128, 128, 129, 129, 129, 129, 129, 129, 130, 130, 130, 130,
            130, 130, 131, 131, 131, 131, 131, 131, 131, 132, 132, 132, 132, 132, 132, 133,
            133, 133, 133, 133, 133, 133, 134, 134, 134, 134, 134, 134, 134, 134, 135, 135,
            135, 135, 135, 135, 135, 135, 136, 136, 136, 136, 136, 136, 136, 136, 137, 137,
            137, 137, 137, 137, 137, 137, 138, 138, 138, 138, 138, 138, 138, 138, 138, 139,
            139, 139, 139, 139, 139, 139, 139, 139, 140, 140, 140, 140, 140, 140, 140, 140,
            140, 141, 141, 141, 141, 141, 141, 141, 141, 141, 141, 142, 142, 142, 142, 142,
            142, 142, 142, 142, 142, 142, 143, 143, 143, 143, 143, 143, 143, 143, 143, 143,
            144,
        ],
        len_sym_cost: 93,
    },
    // match_prob = 0.75 (many matches)
    DefaultLitlenCosts {
        used_lits_to_lit_cost: [
            32, 32, 48, 57, 64, 69, 73, 76, 80, 82, 85, 87, 89, 91, 92, 94,
            96, 97, 98, 99, 101, 102, 103, 104, 105, 106, 107, 108, 108, 109, 110, 111,
            112, 112, 113, 114, 114, 115, 115, 116, 117, 117, 118, 118, 119, 119, 120, 120,
            121, 121, 122, 122, 123, 123, 124, 124, 124, 125, 125, 126, 126, 126, 127, 127,
            128, 128, 128, 129, 129, 129, 130, 130, 130, 131, 131, 131, 131, 132, 132, 132,
            133, 133, 133, 134, 134, 134, 134, 135, 135, 135, 135, 136, 136, 136, 136, 137,
            137, 137, 137, 138, 138, 138, 138, 138, 139, 139, 139, 139, 140, 140, 140, 140,
            140, 141, 141, 141, 141, 141, 142, 142, 142, 142, 142, 143, 143, 143, 143, 143,
            144, 144, 144, 144, 144, 144, 145, 145, 145, 145, 145, 145, 146, 146, 146, 146,
            146, 146, 147, 147, 147, 147, 147, 147, 147, 148, 148, 148, 148, 148, 148, 149,
            149, 149, 149, 149, 149, 149, 150, 150, 150, 150, 150, 150, 150, 150, 151, 151,
            151, 151, 151, 151, 151, 151, 152, 152, 152, 152, 152, 152, 152, 152, 153, 153,
            153, 153, 153, 153, 153, 153, 154, 154, 154, 154, 154, 154, 154, 154, 154, 155,
            155, 155, 155, 155, 155, 155, 155, 155, 156, 156, 156, 156, 156, 156, 156, 156,
            156, 157, 157, 157, 157, 157, 157, 157, 157, 157, 157, 158, 158, 158, 158, 158,
            158, 158, 158, 158, 158, 158, 159, 159, 159, 159, 159, 159, 159, 159, 159, 159,
            160,
        ],
        len_sym_cost: 84,
    },
];

/// Choose default literal and length symbol costs based on data characteristics.
fn choose_default_litlen_costs(
    freqs: &mut DeflateFreqs,
    match_len_freqs: &[u32],
    max_search_depth: u32,
    block_begin: &[u8],
    block_length: u32,
) -> (u32, u32) {
    // Count distinct literals
    freqs.litlen[..DEFLATE_NUM_LITERALS as usize].fill(0);
    for &b in &block_begin[..block_length as usize] {
        freqs.litlen[b as usize] += 1;
    }
    let cutoff = block_length >> 11;
    let mut num_used_literals = 0u32;
    for &f in &freqs.litlen[..DEFLATE_NUM_LITERALS as usize] {
        if f > cutoff {
            num_used_literals += 1;
        }
    }
    if num_used_literals == 0 {
        num_used_literals = 1;
    }

    // Estimate match vs literal frequency
    let mut match_freq = 0u32;
    let mut literal_freq = block_length;
    let min_len = choose_min_match_len(num_used_literals, max_search_depth);
    for (i, &freq) in match_len_freqs.iter().enumerate().skip(min_len as usize) {
        match_freq += freq;
        literal_freq = literal_freq.saturating_sub(i as u32 * freq);
    }

    let table_idx = if match_freq > literal_freq {
        2 // many matches
    } else if match_freq * 4 > literal_freq {
        1 // neutral
    } else {
        0 // few matches
    };

    let lit_cost =
        DEFAULT_LITLEN_COSTS[table_idx].used_lits_to_lit_cost[num_used_literals as usize] as u32;
    let len_sym_cost = DEFAULT_LITLEN_COSTS[table_idx].len_sym_cost as u32;
    (lit_cost, len_sym_cost)
}

/// Cost for a given match length using default costs.
#[inline(always)]
fn default_length_cost(len: u32, len_sym_cost: u32) -> u32 {
    let slot = LENGTH_SLOT[len as usize] as usize;
    let extra = DEFLATE_LENGTH_EXTRA_BITS[slot] as u32;
    len_sym_cost + extra * BIT_COST
}

/// Cost for a given offset slot using default costs.
#[inline(always)]
fn default_offset_slot_cost(slot: usize) -> u32 {
    let extra = DEFLATE_OFFSET_EXTRA_BITS[slot] as u32;
    // Assume all offset symbols equally probable: -log2(1/30) * BIT_COST
    let offset_sym_cost = 4 * BIT_COST + (907 * BIT_COST) / 1000;
    offset_sym_cost + extra * BIT_COST
}

/// Set all costs to default values.
fn set_default_costs(costs: &mut DeflateCosts, lit_cost: u32, len_sym_cost: u32) {
    costs.literal.fill(lit_cost);
    for i in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
        costs.length[i as usize] = default_length_cost(i, len_sym_cost);
    }
    for i in 0..30 {
        costs.offset_slot[i] = default_offset_slot_cost(i);
    }
}

/// Blend a cost toward a default value based on change_amount.
#[inline(always)]
fn adjust_cost(cost: &mut u32, default_cost: u32, change_amount: i32) {
    *cost = match change_amount {
        0 => (default_cost + 3 * *cost) / 4,
        1 => (default_cost + *cost) / 2,
        2 => (5 * default_cost + 3 * *cost) / 8,
        _ => (3 * default_cost + *cost) / 4,
    };
}

/// Adjust all costs toward default values.
fn adjust_costs_impl(
    costs: &mut DeflateCosts,
    lit_cost: u32,
    len_sym_cost: u32,
    change_amount: i32,
) {
    for c in costs.literal.iter_mut() {
        adjust_cost(c, lit_cost, change_amount);
    }
    for i in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
        adjust_cost(
            &mut costs.length[i as usize],
            default_length_cost(i, len_sym_cost),
            change_amount,
        );
    }
    for i in 0..30 {
        adjust_cost(
            &mut costs.offset_slot[i],
            default_offset_slot_cost(i),
            change_amount,
        );
    }
}

/// Adjust costs based on how different the current block is from the previous.
fn adjust_costs(
    costs: &mut DeflateCosts,
    prev_observations: &[u32],
    prev_num_observations: u32,
    split_stats: &BlockSplitStats,
    lit_cost: u32,
    len_sym_cost: u32,
) {
    let mut total_delta = 0u64;
    for (po, so) in prev_observations[..NUM_OBSERVATION_TYPES]
        .iter()
        .zip(&split_stats.observations[..NUM_OBSERVATION_TYPES])
    {
        let prev = *po as u64 * split_stats.num_observations as u64;
        let cur = *so as u64 * prev_num_observations as u64;
        total_delta += prev.abs_diff(cur);
    }
    let cutoff = prev_num_observations as u64 * split_stats.num_observations as u64 * 200 / 512;

    if total_delta > 3 * cutoff {
        set_default_costs(costs, lit_cost, len_sym_cost);
    } else if 4 * total_delta > 9 * cutoff {
        adjust_costs_impl(costs, lit_cost, len_sym_cost, 3);
    } else if 2 * total_delta > 3 * cutoff {
        adjust_costs_impl(costs, lit_cost, len_sym_cost, 2);
    } else if 2 * total_delta > cutoff {
        adjust_costs_impl(costs, lit_cost, len_sym_cost, 1);
    } else {
        adjust_costs_impl(costs, lit_cost, len_sym_cost, 0);
    }
}

/// Set initial costs for a block.
pub(crate) fn set_initial_costs(
    ns: &mut NearOptimalState,
    freqs: &mut DeflateFreqs,
    split_stats: &BlockSplitStats,
    max_search_depth: u32,
    block_begin: &[u8],
    block_length: u32,
    is_first_block: bool,
) {
    let (lit_cost, len_sym_cost) = choose_default_litlen_costs(
        freqs,
        &ns.match_len_freqs,
        max_search_depth,
        block_begin,
        block_length,
    );
    if is_first_block {
        set_default_costs(&mut ns.costs, lit_cost, len_sym_cost);
    } else {
        adjust_costs(
            &mut ns.costs,
            &ns.prev_observations,
            ns.prev_num_observations,
            split_stats,
            lit_cost,
            len_sym_cost,
        );
    }
}

/// Set costs from actual Huffman code lengths.
pub(crate) fn set_costs_from_codes(costs: &mut DeflateCosts, codes: &DeflateCodes) {
    // Literals
    for i in 0..DEFLATE_NUM_LITERALS as usize {
        let bits = if codes.lens_litlen[i] != 0 {
            codes.lens_litlen[i] as u32
        } else {
            LITERAL_NOSTAT_BITS
        };
        costs.literal[i] = bits * BIT_COST;
    }
    // Lengths
    for i in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
        let slot = LENGTH_SLOT[i as usize] as usize;
        let sym = DEFLATE_FIRST_LEN_SYM as usize + slot;
        let bits = if codes.lens_litlen[sym] != 0 {
            codes.lens_litlen[sym] as u32
        } else {
            LENGTH_NOSTAT_BITS
        };
        costs.length[i as usize] = (bits + DEFLATE_LENGTH_EXTRA_BITS[slot] as u32) * BIT_COST;
    }
    // Offset slots
    for (i, (&extra, &len)) in DEFLATE_OFFSET_EXTRA_BITS[..30]
        .iter()
        .zip(&codes.lens_offset[..30])
        .enumerate()
    {
        let bits = if len != 0 {
            len as u32
        } else {
            OFFSET_NOSTAT_BITS
        };
        costs.offset_slot[i] = (bits + extra as u32) * BIT_COST;
    }
}

// ---- Path finding ----

/// Walk the optimum path and tally symbol frequencies.
#[cfg(not(feature = "unchecked"))]
fn tally_item_list(
    nodes: &[OptimumNode],
    block_length: u32,
    offset_slot_full: &[u8],
    freqs: &mut DeflateFreqs,
) {
    let mut cur_idx = 0usize;
    let end = block_length as usize;
    while cur_idx < end {
        let length = nodes[cur_idx].item & OPTIMUM_LEN_MASK;
        let offset = nodes[cur_idx].item >> OPTIMUM_OFFSET_SHIFT;
        if length == 1 {
            freqs.litlen[offset as usize] += 1;
        } else {
            let len_slot = LENGTH_SLOT[length as usize] as usize;
            freqs.litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] += 1;
            freqs.offset[offset_slot_full[offset as usize] as usize] += 1;
        }
        cur_idx += length as usize;
    }
    freqs.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;
}

/// Walk the optimum path and tally symbol frequencies (raw-pointer variant).
#[cfg(feature = "unchecked")]
fn tally_item_list(
    nodes: &[OptimumNode],
    block_length: u32,
    offset_slot_full: &[u8],
    freqs: &mut DeflateFreqs,
) {
    let nodes_ptr = nodes.as_ptr();
    let osf_ptr = offset_slot_full.as_ptr();
    let mut cur_idx = 0usize;
    let end = block_length as usize;
    // SAFETY: cur_idx bounded by block_length <= MAX_BLOCK_LENGTH < nodes.len().
    // offset bounded by DEFLATE_MAX_MATCH_OFFSET < offset_slot_full.len().
    unsafe {
        while cur_idx < end {
            let node = &*nodes_ptr.add(cur_idx);
            let length = node.item & OPTIMUM_LEN_MASK;
            let offset = node.item >> OPTIMUM_OFFSET_SHIFT;
            if length == 1 {
                freqs.litlen[offset as usize] += 1;
            } else {
                let len_slot = LENGTH_SLOT[length as usize] as usize;
                freqs.litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] += 1;
                freqs.offset[*osf_ptr.add(offset as usize) as usize] += 1;
            }
            cur_idx += length as usize;
        }
    }
    freqs.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;
}

/// Set up all-literals encoding: reset freqs, count literals, build codes.
fn choose_all_literals(
    block_begin: &[u8],
    block_length: u32,
    freqs: &mut DeflateFreqs,
    codes: &mut DeflateCodes,
) {
    freqs.reset();
    for &b in &block_begin[..block_length as usize] {
        freqs.litlen[b as usize] += 1;
    }
    freqs.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;
    make_huffman_codes(freqs, codes);
}

/// Compute the true (exact) dynamic block cost from freqs and codes.
///
/// Includes the dynamic Huffman header cost.
fn compute_true_cost(freqs: &DeflateFreqs, codes: &DeflateCodes) -> u32 {
    let mut cost = 0u32;

    // Count active symbols
    let mut num_litlen_syms = DEFLATE_NUM_LITLEN_SYMS as usize;
    while num_litlen_syms > 257 && codes.lens_litlen[num_litlen_syms - 1] == 0 {
        num_litlen_syms -= 1;
    }
    let mut num_offset_syms = DEFLATE_NUM_OFFSET_SYMS as usize;
    while num_offset_syms > 1 && codes.lens_offset[num_offset_syms - 1] == 0 {
        num_offset_syms -= 1;
    }

    // Build combined lens array for precode
    let total_lens = num_litlen_syms + num_offset_syms;
    let mut combined_lens = [0u8; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
    combined_lens[..num_litlen_syms].copy_from_slice(&codes.lens_litlen[..num_litlen_syms]);
    combined_lens[num_litlen_syms..num_litlen_syms + num_offset_syms]
        .copy_from_slice(&codes.lens_offset[..num_offset_syms]);

    // Compute precode
    let mut precode_freqs = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_items = [0u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
    compute_precode_items(
        &combined_lens[..total_lens],
        &mut precode_freqs,
        &mut precode_items,
    );

    let mut precode_lens = [0u8; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_codewords = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    make_huffman_code(
        DEFLATE_NUM_PRECODE_SYMS as usize,
        DEFLATE_MAX_PRE_CODEWORD_LEN,
        &precode_freqs,
        &mut precode_lens,
        &mut precode_codewords,
    );
    let _ = precode_codewords; // only need lens for cost

    let mut num_explicit_lens = DEFLATE_NUM_PRECODE_SYMS as usize;
    while num_explicit_lens > 4
        && precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[num_explicit_lens - 1] as usize] == 0
    {
        num_explicit_lens -= 1;
    }

    // Header cost
    cost += 5 + 5 + 4 + 3 * num_explicit_lens as u32;
    for sym in 0..DEFLATE_NUM_PRECODE_SYMS as usize {
        cost += precode_freqs[sym] * (precode_lens[sym] as u32 + EXTRA_PRECODE_BITS[sym] as u32);
    }

    // Literal cost
    for sym in 0..DEFLATE_FIRST_LEN_SYM as usize {
        cost += freqs.litlen[sym] * codes.lens_litlen[sym] as u32;
    }

    // Length symbol cost
    for (i, &extra) in DEFLATE_LENGTH_EXTRA_BITS.iter().enumerate() {
        let sym = DEFLATE_FIRST_LEN_SYM as usize + i;
        cost += freqs.litlen[sym] * (codes.lens_litlen[sym] as u32 + extra as u32);
    }

    // Offset symbol cost
    for (sym, &extra) in DEFLATE_OFFSET_EXTRA_BITS[..30].iter().enumerate() {
        cost += freqs.offset[sym] * (codes.lens_offset[sym] as u32 + extra as u32);
    }

    cost
}

/// Find the minimum-cost literal/match path through the block.
///
/// Uses backward dynamic programming. After completion, `optimum_nodes[0].item`
/// starts the optimal path. Also sets freqs and codes from the path.
#[cfg(not(feature = "unchecked"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_min_cost_path(
    optimum_nodes: &mut [OptimumNode],
    costs: &DeflateCosts,
    offset_slot_full: &[u8],
    block_length: u32,
    match_cache: &[LzMatch],
    cache_end: usize,
    freqs: &mut DeflateFreqs,
    codes: &mut DeflateCodes,
) {
    let end = block_length as usize;
    optimum_nodes[end].cost_to_end = 0;

    let mut cache_idx = cache_end;
    let mut cur_idx = end;

    while cur_idx > 0 {
        cur_idx -= 1;
        cache_idx -= 1;

        let num_matches = match_cache[cache_idx].length as usize;
        let literal = match_cache[cache_idx].offset as u32;

        // Literal option
        let mut best_cost =
            costs.literal[literal as usize] + optimum_nodes[cur_idx + 1].cost_to_end;
        optimum_nodes[cur_idx].item = (literal << OPTIMUM_OFFSET_SHIFT) | 1;

        // Match options
        if num_matches > 0 {
            let match_start = cache_idx - num_matches;
            let mut match_idx = match_start;
            let mut len = DEFLATE_MIN_MATCH_LEN;

            loop {
                let offset = match_cache[match_idx].offset as u32;
                let os_idx = offset_slot_full[offset as usize] as usize;
                let offset_cost = costs.offset_slot[os_idx];

                loop {
                    let cost = offset_cost
                        + costs.length[len as usize]
                        + optimum_nodes[cur_idx + len as usize].cost_to_end;
                    if cost < best_cost {
                        best_cost = cost;
                        optimum_nodes[cur_idx].item = len | (offset << OPTIMUM_OFFSET_SHIFT);
                    }
                    len += 1;
                    if len > match_cache[match_idx].length as u32 {
                        break;
                    }
                }

                match_idx += 1;
                if match_idx == cache_idx {
                    break;
                }
            }
            cache_idx -= num_matches;
        }

        optimum_nodes[cur_idx].cost_to_end = best_cost;
    }

    // Tally frequencies from the optimal path and build codes
    freqs.reset();
    tally_item_list(optimum_nodes, block_length, offset_slot_full, freqs);
    make_huffman_codes(freqs, codes);
}

/// Find the minimum-cost literal/match path (raw-pointer variant).
///
/// Eliminates 3 fat pointers (optimum_nodes, match_cache, offset_slot_full)
/// from the inner DP loop, freeing 6 registers on x86-64.
#[cfg(feature = "unchecked")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn find_min_cost_path(
    optimum_nodes: &mut [OptimumNode],
    costs: &DeflateCosts,
    offset_slot_full: &[u8],
    block_length: u32,
    match_cache: &[LzMatch],
    cache_end: usize,
    freqs: &mut DeflateFreqs,
    codes: &mut DeflateCodes,
) {
    let end = block_length as usize;

    let nodes_ptr = optimum_nodes.as_mut_ptr();
    let cache_ptr = match_cache.as_ptr();
    let osf_ptr = offset_slot_full.as_ptr();

    // SAFETY: All indices are bounded by block_length <= MAX_BLOCK_LENGTH
    // (< optimum_nodes.len()), cache_end <= match_cache.len(), and
    // offsets <= DEFLATE_MAX_MATCH_OFFSET (< offset_slot_full.len()).
    // The costs arrays use fixed-size types with known bounds.
    unsafe {
        (*nodes_ptr.add(end)).cost_to_end = 0;

        let mut cache_idx = cache_end;
        let mut cur_idx = end;

        while cur_idx > 0 {
            cur_idx -= 1;
            cache_idx -= 1;

            let cache_entry = &*cache_ptr.add(cache_idx);
            let num_matches = cache_entry.length as usize;
            let literal = cache_entry.offset as u32;

            // Literal option
            let mut best_cost = *costs.literal.get_unchecked(literal as usize)
                + (*nodes_ptr.add(cur_idx + 1)).cost_to_end;
            (*nodes_ptr.add(cur_idx)).item = (literal << OPTIMUM_OFFSET_SHIFT) | 1;

            // Match options
            if num_matches > 0 {
                let mut match_idx = cache_idx - num_matches;
                let mut len = DEFLATE_MIN_MATCH_LEN;

                loop {
                    let m = &*cache_ptr.add(match_idx);
                    let offset = m.offset as u32;
                    let os_idx = *osf_ptr.add(offset as usize) as usize;
                    let offset_cost = *costs.offset_slot.get_unchecked(os_idx);

                    loop {
                        let cost = offset_cost
                            + *costs.length.get_unchecked(len as usize)
                            + (*nodes_ptr.add(cur_idx + len as usize)).cost_to_end;
                        if cost < best_cost {
                            best_cost = cost;
                            (*nodes_ptr.add(cur_idx)).item = len | (offset << OPTIMUM_OFFSET_SHIFT);
                        }
                        len += 1;
                        if len > m.length as u32 {
                            break;
                        }
                    }

                    match_idx += 1;
                    if match_idx == cache_idx {
                        break;
                    }
                }
                cache_idx -= num_matches;
            }

            (*nodes_ptr.add(cur_idx)).cost_to_end = best_cost;
        }
    }

    // Tally frequencies from the optimal path and build codes
    freqs.reset();
    tally_item_list(optimum_nodes, block_length, offset_slot_full, freqs);
    make_huffman_codes(freqs, codes);
}

// ---- Block optimization ----

/// Optimize and flush a near-optimal block.
///
/// Runs multiple optimization passes, considers literal-only and static blocks,
/// then flushes using the best approach.
///
/// Returns true if the block used only literals (no matches).
#[allow(clippy::too_many_arguments)]
pub(crate) fn optimize_and_flush_block(
    ns: &mut NearOptimalState,
    os: &mut OutputBitstream<'_>,
    block_begin: &[u8],
    block_length: u32,
    cache_end: usize,
    is_first_block: bool,
    is_final_block: bool,
    freqs: &mut DeflateFreqs,
    codes: &mut DeflateCodes,
    static_codes: &DeflateCodes,
    split_stats: &BlockSplitStats,
    max_search_depth: u32,
) -> bool {
    let mut num_passes_remaining = ns.max_optim_passes;
    let mut best_true_cost = u32::MAX;

    // Consider all-literals encoding
    choose_all_literals(block_begin, block_length, freqs, codes);
    let only_lits_cost = compute_true_cost(freqs, codes);

    // Force the block to end at the desired length (prevent match overshoot)
    let sentinel_end =
        (block_length as usize + DEFLATE_MAX_MATCH_LEN as usize).min(ns.optimum_nodes.len() - 1);
    for i in block_length as usize..=sentinel_end {
        ns.optimum_nodes[i].cost_to_end = 0x80000000;
    }

    // Consider static Huffman for small blocks
    let mut static_cost = u32::MAX;
    if block_length <= ns.max_len_to_optimize_static_block {
        ns.costs_saved = ns.costs.clone();
        set_costs_from_codes(&mut ns.costs, static_codes);
        find_min_cost_path(
            &mut ns.optimum_nodes,
            &ns.costs,
            &ns.offset_slot_full,
            block_length,
            &ns.match_cache,
            cache_end,
            freqs,
            codes,
        );
        static_cost = ns.optimum_nodes[0].cost_to_end / BIT_COST;
        static_cost += 7; // end-of-block symbol cost for static
        ns.costs = ns.costs_saved.clone();
    }

    // Initialize costs
    set_initial_costs(
        ns,
        freqs,
        split_stats,
        max_search_depth,
        block_begin,
        block_length,
        is_first_block,
    );

    // Iterative optimization loop
    loop {
        find_min_cost_path(
            &mut ns.optimum_nodes,
            &ns.costs,
            &ns.offset_slot_full,
            block_length,
            &ns.match_cache,
            cache_end,
            freqs,
            codes,
        );

        let true_cost = compute_true_cost(freqs, codes);

        if true_cost + ns.min_improvement_to_continue > best_true_cost {
            break;
        }
        best_true_cost = true_cost;
        ns.costs_saved = ns.costs.clone();
        set_costs_from_codes(&mut ns.costs, codes);

        num_passes_remaining -= 1;
        if num_passes_remaining == 0 {
            break;
        }
    }

    // Choose the best approach
    let used_only_literals = false;
    let true_cost = compute_true_cost(freqs, codes);

    if only_lits_cost.min(static_cost) < best_true_cost {
        if only_lits_cost < static_cost {
            // Literal-only is best
            choose_all_literals(block_begin, block_length, freqs, codes);
            set_costs_from_codes(&mut ns.costs, codes);

            let seq = Sequence {
                litrunlen_and_length: block_length,
                offset: 0,
                offset_slot: 0,
            };
            flush_block(
                os,
                block_begin,
                block_length as usize,
                BlockOutput::Sequences(&[seq]),
                freqs,
                codes,
                static_codes,
                is_final_block,
            );
            return true;
        } else {
            // Static block is best — recompute with static costs
            set_costs_from_codes(&mut ns.costs, static_codes);
            find_min_cost_path(
                &mut ns.optimum_nodes,
                &ns.costs,
                &ns.offset_slot_full,
                block_length,
                &ns.match_cache,
                cache_end,
                freqs,
                codes,
            );
        }
    } else if true_cost >= best_true_cost + ns.min_bits_to_use_nonfinal_path {
        // Recover the non-final pass's path
        ns.costs = ns.costs_saved.clone();
        find_min_cost_path(
            &mut ns.optimum_nodes,
            &ns.costs,
            &ns.offset_slot_full,
            block_length,
            &ns.match_cache,
            cache_end,
            freqs,
            codes,
        );
        set_costs_from_codes(&mut ns.costs, codes);
    }

    flush_block(
        os,
        block_begin,
        block_length as usize,
        BlockOutput::Optimum {
            nodes: &ns.optimum_nodes,
            block_length: block_length as usize,
            offset_slot_full: &ns.offset_slot_full,
        },
        freqs,
        codes,
        static_codes,
        is_final_block,
    );

    used_only_literals
}

// ---- Statistics helpers ----

/// Initialize stats for a new compression run.
pub(crate) fn init_stats(split_stats: &mut BlockSplitStats, ns: &mut NearOptimalState) {
    *split_stats = BlockSplitStats::new();
    ns.new_match_len_freqs.fill(0);
    ns.match_len_freqs.fill(0);
}

/// Merge new stats into accumulated stats.
pub(crate) fn merge_stats(split_stats: &mut BlockSplitStats, ns: &mut NearOptimalState) {
    split_stats.merge_new_observations();
    for i in 0..ns.match_len_freqs.len() {
        ns.match_len_freqs[i] += ns.new_match_len_freqs[i];
        ns.new_match_len_freqs[i] = 0;
    }
}

/// Save current stats as "previous" for next block.
pub(crate) fn save_stats(split_stats: &BlockSplitStats, ns: &mut NearOptimalState) {
    for i in 0..NUM_OBSERVATION_TYPES {
        ns.prev_observations[i] = split_stats.observations[i];
    }
    ns.prev_num_observations = split_stats.num_observations;
}

/// Clear old stats, keeping only new (for partial block reset).
pub(crate) fn clear_old_stats(split_stats: &mut BlockSplitStats, ns: &mut NearOptimalState) {
    split_stats.observations.fill(0);
    split_stats.num_observations = 0;
    ns.match_len_freqs.fill(0);
}

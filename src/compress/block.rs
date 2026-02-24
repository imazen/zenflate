//! Block flushing: choose best block type and encode it.
//!
//! Ported from libdeflate's `deflate_flush_block()`, `deflate_precompute_huffman_header()`,
//! `deflate_compute_precode_items()`, `deflate_compute_full_len_codewords()`.

use crate::constants::*;

use super::bitstream::{BITBUF_NBITS, OutputBitstream, can_buffer};
use super::huffman::make_huffman_code;
use super::near_optimal::{OPTIMUM_LEN_MASK, OPTIMUM_OFFSET_SHIFT, OptimumNode};
use super::sequences::Sequence;

/// Source of output items for block flushing.
pub(crate) enum BlockOutput<'a> {
    /// Traditional sequence-based output (greedy/lazy/fastest).
    Sequences(&'a [Sequence]),
    /// Near-optimal output: walk optimum_nodes directly.
    Optimum {
        nodes: &'a [OptimumNode],
        block_length: usize,
        offset_slot_full: &'a [u8],
    },
}

/// Codes: Huffman codewords and lengths for litlen + offset alphabets.
#[derive(Clone)]
pub(crate) struct DeflateCodes {
    pub codewords_litlen: [u32; DEFLATE_NUM_LITLEN_SYMS as usize],
    pub codewords_offset: [u32; DEFLATE_NUM_OFFSET_SYMS as usize],
    pub lens_litlen: [u8; DEFLATE_NUM_LITLEN_SYMS as usize],
    pub lens_offset: [u8; DEFLATE_NUM_OFFSET_SYMS as usize],
}

impl Default for DeflateCodes {
    fn default() -> Self {
        Self {
            codewords_litlen: [0; DEFLATE_NUM_LITLEN_SYMS as usize],
            codewords_offset: [0; DEFLATE_NUM_OFFSET_SYMS as usize],
            lens_litlen: [0; DEFLATE_NUM_LITLEN_SYMS as usize],
            lens_offset: [0; DEFLATE_NUM_OFFSET_SYMS as usize],
        }
    }
}

/// Symbol frequency counters.
#[derive(Clone)]
pub(crate) struct DeflateFreqs {
    pub litlen: [u32; DEFLATE_NUM_LITLEN_SYMS as usize],
    pub offset: [u32; DEFLATE_NUM_OFFSET_SYMS as usize],
}

impl Default for DeflateFreqs {
    fn default() -> Self {
        Self {
            litlen: [0; DEFLATE_NUM_LITLEN_SYMS as usize],
            offset: [0; DEFLATE_NUM_OFFSET_SYMS as usize],
        }
    }
}

impl DeflateFreqs {
    pub fn reset(&mut self) {
        self.litlen.fill(0);
        self.offset.fill(0);
    }
}

/// Extra bits for each precode symbol.
pub(crate) const EXTRA_PRECODE_BITS: [u8; DEFLATE_NUM_PRECODE_SYMS as usize] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];

/// Length slot for each match length (3..=258).
#[rustfmt::skip]
pub(crate) const LENGTH_SLOT: [u8; DEFLATE_MAX_MATCH_LEN as usize + 1] = {
    let mut table = [0u8; DEFLATE_MAX_MATCH_LEN as usize + 1];
    // Fill from the length base/extra tables
    let mut slot = 0u8;
    while slot < 29 {
        let base = DEFLATE_LENGTH_BASE[slot as usize] as usize;
        let extra = DEFLATE_LENGTH_EXTRA_BITS[slot as usize];
        let count = 1usize << extra;
        let mut j = 0usize;
        while j < count && base + j <= DEFLATE_MAX_MATCH_LEN as usize {
            table[base + j] = slot;
            j += 1;
        }
        slot += 1;
    }
    table
};

/// Offset slot for offset-1 in [0..255].
/// Computed from offset base/extra tables.
#[allow(dead_code)]
const OFFSET_SLOT_SMALL: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut slot = 0u8;
    while slot < 30 {
        let base = DEFLATE_OFFSET_BASE[slot as usize] as usize;
        let extra = DEFLATE_OFFSET_EXTRA_BITS[slot as usize];
        let count = 1usize << extra;
        let mut j = 0usize;
        while j < count {
            let offset_m1 = base + j - 1; // offset - 1
            if offset_m1 < 256 {
                table[offset_m1] = slot;
            }
            j += 1;
        }
        slot += 1;
    }
    table
};

/// Get the offset slot for a given match offset (1..=32768).
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn get_offset_slot(offset: u32) -> u32 {
    debug_assert!((1..=32768).contains(&offset));
    let n = (256u32.wrapping_sub(offset)) >> 29;
    OFFSET_SLOT_SMALL[((offset - 1) >> n) as usize] as u32 + (n << 1)
}

/// Build litlen and offset Huffman codes from frequency tables.
pub(crate) fn make_huffman_codes(freqs: &DeflateFreqs, codes: &mut DeflateCodes) {
    make_huffman_code(
        DEFLATE_NUM_LITLEN_SYMS as usize,
        MAX_LITLEN_CODEWORD_LEN,
        &freqs.litlen,
        &mut codes.lens_litlen,
        &mut codes.codewords_litlen,
    );
    make_huffman_code(
        DEFLATE_NUM_OFFSET_SYMS as usize,
        DEFLATE_MAX_OFFSET_CODEWORD_LEN,
        &freqs.offset,
        &mut codes.lens_offset,
        &mut codes.codewords_offset,
    );
}

/// Initialize the static Huffman codes defined by the DEFLATE format.
pub(crate) fn init_static_codes(freqs: &mut DeflateFreqs, codes: &mut DeflateCodes) {
    #[allow(clippy::eq_op)]
    {
        freqs.litlen[..144].fill(1 << (9 - 8));
        freqs.litlen[144..256].fill(1 << (9 - 9));
        freqs.litlen[256..280].fill(1 << (9 - 7));
        freqs.litlen[280..288].fill(1 << (9 - 8));
        freqs.offset[..32].fill(1 << (5 - 5));
    }
    make_huffman_codes(freqs, codes);
}

/// Flags controlling which RLE codes are used in precode encoding.
#[derive(Clone, Copy)]
pub(crate) struct PrecodeFlags {
    /// Allow RLE code 16 (repeat previous non-zero length, 3-6 times).
    pub use_16: bool,
    /// Allow RLE code 17 (repeat zero, 3-10 times).
    pub use_17: bool,
    /// Allow RLE code 18 (repeat zero, 11-138 times).
    pub use_18: bool,
    /// When a non-zero symbol repeats exactly 7 times, encode as two code-16
    /// runs (4+3) instead of one code-16 run (6) + 1 literal.
    pub fuse_7: bool,
    /// When a non-zero symbol repeats exactly 8 times, encode as two code-16
    /// runs (4+4) instead of one code-16 run (6) + 2 literals.
    pub fuse_8: bool,
}

impl PrecodeFlags {
    /// Default flags: all RLE codes enabled, no fusing.
    pub const DEFAULT: Self = Self {
        use_16: true,
        use_17: true,
        use_18: true,
        fuse_7: false,
        fuse_8: false,
    };

    /// Construct from a bitmask (5 bits: use_16, use_17, use_18, fuse_7, fuse_8).
    pub fn from_bits(bits: u8) -> Self {
        Self {
            use_16: bits & 1 != 0,
            use_17: bits & 2 != 0,
            use_18: bits & 4 != 0,
            fuse_7: bits & 8 != 0,
            fuse_8: bits & 16 != 0,
        }
    }

    /// Check if this flag combination is valid.
    /// fuse_7/fuse_8 require use_16.
    pub fn is_valid(self) -> bool {
        if (self.fuse_7 || self.fuse_8) && !self.use_16 {
            return false;
        }
        true
    }
}

/// Compute RLE-encoded precode items for the combined lens array.
///
/// Returns the number of items written to `precode_items`.
pub(crate) fn compute_precode_items(
    lens: &[u8],
    precode_freqs: &mut [u32; DEFLATE_NUM_PRECODE_SYMS as usize],
    precode_items: &mut [u32],
) -> usize {
    compute_precode_items_flagged(lens, precode_freqs, precode_items, PrecodeFlags::DEFAULT)
}

/// Compute RLE-encoded precode items with configurable RLE strategy flags.
///
/// Returns the number of items written to `precode_items`.
pub(crate) fn compute_precode_items_flagged(
    lens: &[u8],
    precode_freqs: &mut [u32; DEFLATE_NUM_PRECODE_SYMS as usize],
    precode_items: &mut [u32],
    flags: PrecodeFlags,
) -> usize {
    precode_freqs.fill(0);
    let num_lens = lens.len();
    let mut item_count = 0;
    let mut run_start = 0;

    while run_start < num_lens {
        let len = lens[run_start];
        let mut run_end = run_start + 1;
        while run_end < num_lens && lens[run_end] == len {
            run_end += 1;
        }

        if len == 0 {
            // Run of zeroes
            // Symbol 18: RLE 11..=138 zeroes
            if flags.use_18 {
                while run_end - run_start >= 11 {
                    let extra_bits = (run_end - run_start - 11).min(0x7F) as u32;
                    precode_freqs[18] += 1;
                    precode_items[item_count] = 18 | (extra_bits << 5);
                    item_count += 1;
                    run_start += 11 + extra_bits as usize;
                }
            }
            // Symbol 17: RLE 3..=10 zeroes
            if flags.use_17 && run_end - run_start >= 3 {
                while run_end - run_start >= 3 {
                    let extra_bits = (run_end - run_start - 3).min(0x7) as u32;
                    precode_freqs[17] += 1;
                    precode_items[item_count] = 17 | (extra_bits << 5);
                    item_count += 1;
                    run_start += 3 + extra_bits as usize;
                }
            }
        } else if flags.use_16 {
            // Run of nonzero lengths with code 16 available
            let run_len = run_end - run_start;

            if flags.fuse_7 && run_len == 7 {
                // Fuse: 1 literal + code16(4) + code16(3) = 7 total
                precode_freqs[len as usize] += 1;
                precode_items[item_count] = len as u32;
                item_count += 1;
                run_start += 1;
                // code16 repeat 4 (extra=1)
                precode_freqs[16] += 1;
                precode_items[item_count] = 16 | (1 << 5);
                item_count += 1;
                run_start += 4;
                // code16 repeat 3 (extra=0)
                precode_freqs[16] += 1;
                precode_items[item_count] = 16;
                item_count += 1;
                run_start += 3;
                // Exact, no remainder - skip fallthrough
                debug_assert_eq!(run_start, run_end);
                continue;
            } else if flags.fuse_8 && run_len == 8 {
                // Fuse: 1 literal + code16(4) + code16(4) = 9... no, 1+4+4=9 != 8
                // Actually: 1 literal + code16(4) + code16(3) = 1+4+3=8. That's fuse_7 for 8?
                // Re-reading plan: fuse_8 = 8 repeats -> two code16 runs (4+4) instead of code16(6) + 2 literals
                // code16(6) + 2 literals = 1 literal + code16(6) + 2 = 9 items?? No.
                // Original: run of 8 = 1 literal + code16(6) + 1 literal = item_count 3 (len + code16(3extra) + len)
                // Actually original for run 8: literal + code16(max=6) leaves 1 remaining = literal + code16(6) + literal
                // Fuse_8: literal + code16(4) + code16(4) = also 3 items but different code16 extra bits
                // 1 literal, then code16(4) + code16(3) = 1+4+3 = 8 total positions
                precode_freqs[len as usize] += 1;
                precode_items[item_count] = len as u32;
                item_count += 1;
                run_start += 1;
                // code16 repeat 4 (extra=1)
                precode_freqs[16] += 1;
                precode_items[item_count] = 16 | (1 << 5);
                item_count += 1;
                run_start += 4;
                // code16 repeat 3 (extra=0)
                precode_freqs[16] += 1;
                precode_items[item_count] = 16;
                item_count += 1;
                run_start += 3;
                debug_assert_eq!(run_start, run_end);
                continue;
            } else if run_len >= 4 {
                // Standard code16: emit 1 literal, then code16 runs
                precode_freqs[len as usize] += 1;
                precode_items[item_count] = len as u32;
                item_count += 1;
                run_start += 1;
                while run_end - run_start >= 3 {
                    let extra_bits = (run_end - run_start - 3).min(0x3) as u32;
                    precode_freqs[16] += 1;
                    precode_items[item_count] = 16 | (extra_bits << 5);
                    item_count += 1;
                    run_start += 3 + extra_bits as usize;
                }
            }
        }

        // Output remaining lengths without RLE.
        while run_start < run_end {
            precode_freqs[len as usize] += 1;
            precode_items[item_count] = len as u32;
            item_count += 1;
            run_start += 1;
        }
    }

    item_count
}

/// Compute the bit cost of a precode encoding without actually writing items.
///
/// Returns the total header bits for a dynamic block's code length section:
/// 14 fixed header bits + 3*hclen + precode symbol costs + extra bits.
fn compute_precode_cost(
    lens: &[u8],
    flags: PrecodeFlags,
) -> u32 {
    let mut precode_freqs = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_items = [0u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
    compute_precode_items_flagged(lens, &mut precode_freqs, &mut precode_items, flags);

    // Build precode Huffman code for this combination
    let mut precode_lens = [0u8; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_codewords = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    make_huffman_code(
        DEFLATE_NUM_PRECODE_SYMS as usize,
        DEFLATE_MAX_PRE_CODEWORD_LEN,
        &precode_freqs,
        &mut precode_lens,
        &mut precode_codewords,
    );
    let _ = precode_codewords;

    // Count how many precode lengths to output (min 4)
    let mut num_explicit_lens = DEFLATE_NUM_PRECODE_SYMS as usize;
    while num_explicit_lens > 4
        && precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[num_explicit_lens - 1] as usize] == 0
    {
        num_explicit_lens -= 1;
    }

    // Total cost: 14 fixed bits + 3*hclen + sum(freq * (len + extra_bits))
    let mut cost = 14u32 + 3 * num_explicit_lens as u32;
    for sym in 0..DEFLATE_NUM_PRECODE_SYMS as usize {
        cost += precode_freqs[sym] * (precode_lens[sym] as u32 + EXTRA_PRECODE_BITS[sym] as u32);
    }

    cost
}

/// Result of the best precode search.
pub(crate) struct BestPrecodeResult {
    pub precode_freqs: [u32; DEFLATE_NUM_PRECODE_SYMS as usize],
    pub precode_items: [u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize],
    pub num_items: usize,
    pub precode_lens: [u8; DEFLATE_NUM_PRECODE_SYMS as usize],
    pub precode_codewords: [u32; DEFLATE_NUM_PRECODE_SYMS as usize],
    pub num_explicit_lens: usize,
    pub cost: u32,
}

/// Search all valid flag combinations and return the best precode encoding.
///
/// Tests up to 24 of 32 combinations (skipping fuse_7/fuse_8 without use_16).
/// Returns the combination with the lowest total header bit cost.
pub(crate) fn compute_precode_items_best(lens: &[u8]) -> BestPrecodeResult {
    let mut best_cost = u32::MAX;
    let mut best_flags = PrecodeFlags::DEFAULT;

    // Search all 32 combinations, skip invalid ones
    for bits in 0..32u8 {
        let flags = PrecodeFlags::from_bits(bits);
        if !flags.is_valid() {
            continue;
        }
        let cost = compute_precode_cost(lens, flags);
        if cost < best_cost {
            best_cost = cost;
            best_flags = flags;
        }
    }

    // Now compute the actual items with the best flags
    let mut result = BestPrecodeResult {
        precode_freqs: [0u32; DEFLATE_NUM_PRECODE_SYMS as usize],
        precode_items: [0u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize],
        num_items: 0,
        precode_lens: [0u8; DEFLATE_NUM_PRECODE_SYMS as usize],
        precode_codewords: [0u32; DEFLATE_NUM_PRECODE_SYMS as usize],
        num_explicit_lens: 0,
        cost: best_cost,
    };

    result.num_items = compute_precode_items_flagged(
        lens,
        &mut result.precode_freqs,
        &mut result.precode_items,
        best_flags,
    );

    make_huffman_code(
        DEFLATE_NUM_PRECODE_SYMS as usize,
        DEFLATE_MAX_PRE_CODEWORD_LEN,
        &result.precode_freqs,
        &mut result.precode_lens,
        &mut result.precode_codewords,
    );

    result.num_explicit_lens = DEFLATE_NUM_PRECODE_SYMS as usize;
    while result.num_explicit_lens > 4
        && result.precode_lens
            [DEFLATE_PRECODE_LENS_PERMUTATION[result.num_explicit_lens - 1] as usize]
            == 0
    {
        result.num_explicit_lens -= 1;
    }

    result
}

/// Flush a complete DEFLATE block.
///
/// Chooses the cheapest block type (uncompressed, static Huffman, dynamic Huffman)
/// and writes it to the output bitstream.
#[allow(clippy::too_many_arguments)]
pub(crate) fn flush_block(
    os: &mut OutputBitstream<'_>,
    block_begin: &[u8],
    block_length: usize,
    output: BlockOutput<'_>,
    freqs: &DeflateFreqs,
    codes: &DeflateCodes,
    static_codes: &DeflateCodes,
    is_final_block: bool,
) {
    flush_block_inner(
        os,
        block_begin,
        block_length,
        output,
        freqs,
        codes,
        static_codes,
        is_final_block,
        false,
    );
}

/// Flush a complete DEFLATE block with optional exhaustive precode search.
///
/// When `use_best_precode` is true, searches all valid RLE flag combinations
/// to find the smallest tree header encoding.
#[allow(clippy::too_many_arguments)]
pub(crate) fn flush_block_best(
    os: &mut OutputBitstream<'_>,
    block_begin: &[u8],
    block_length: usize,
    output: BlockOutput<'_>,
    freqs: &DeflateFreqs,
    codes: &DeflateCodes,
    static_codes: &DeflateCodes,
    is_final_block: bool,
) {
    flush_block_inner(
        os,
        block_begin,
        block_length,
        output,
        freqs,
        codes,
        static_codes,
        is_final_block,
        true,
    );
}

/// Inner implementation of flush_block with optional best-precode search.
#[allow(clippy::too_many_arguments)]
fn flush_block_inner(
    os: &mut OutputBitstream<'_>,
    block_begin: &[u8],
    block_length: usize,
    output: BlockOutput<'_>,
    freqs: &DeflateFreqs,
    codes: &DeflateCodes,
    static_codes: &DeflateCodes,
    is_final_block: bool,
    use_best_precode: bool,
) {
    let in_data = &block_begin[..block_length];

    // ---- Precompute precode items ----

    // Count how many litlen and offset symbols we need
    let mut num_litlen_syms = DEFLATE_NUM_LITLEN_SYMS as usize;
    while num_litlen_syms > 257 && codes.lens_litlen[num_litlen_syms - 1] == 0 {
        num_litlen_syms -= 1;
    }
    let mut num_offset_syms = DEFLATE_NUM_OFFSET_SYMS as usize;
    while num_offset_syms > 1 && codes.lens_offset[num_offset_syms - 1] == 0 {
        num_offset_syms -= 1;
    }

    // Build contiguous lens array for precode encoding
    let total_lens = num_litlen_syms + num_offset_syms;
    let mut combined_lens = [0u8; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
    combined_lens[..num_litlen_syms].copy_from_slice(&codes.lens_litlen[..num_litlen_syms]);
    combined_lens[num_litlen_syms..num_litlen_syms + num_offset_syms]
        .copy_from_slice(&codes.lens_offset[..num_offset_syms]);

    // Compute precode items (RLE tokens) — optionally with exhaustive search
    let mut precode_freqs;
    let mut precode_items;
    let num_precode_items;
    let mut precode_lens;
    let mut precode_codewords;
    let num_explicit_lens;

    if use_best_precode {
        let best = compute_precode_items_best(&combined_lens[..total_lens]);
        precode_freqs = best.precode_freqs;
        precode_items = best.precode_items;
        num_precode_items = best.num_items;
        precode_lens = best.precode_lens;
        precode_codewords = best.precode_codewords;
        num_explicit_lens = best.num_explicit_lens;
    } else {
        precode_freqs = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
        precode_items =
            [0u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
        num_precode_items = compute_precode_items(
            &combined_lens[..total_lens],
            &mut precode_freqs,
            &mut precode_items,
        );

        precode_lens = [0u8; DEFLATE_NUM_PRECODE_SYMS as usize];
        precode_codewords = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
        make_huffman_code(
            DEFLATE_NUM_PRECODE_SYMS as usize,
            DEFLATE_MAX_PRE_CODEWORD_LEN,
            &precode_freqs,
            &mut precode_lens,
            &mut precode_codewords,
        );

        num_explicit_lens = {
            let mut n = DEFLATE_NUM_PRECODE_SYMS as usize;
            while n > 4
                && precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[n - 1] as usize] == 0
            {
                n -= 1;
            }
            n
        };
    }

    // ---- Compute block costs ----

    let bitcount = os.bitcount;
    let mut dynamic_cost = 3u64;
    let mut static_cost = 3u64;

    // Dynamic Huffman header cost
    dynamic_cost += 5 + 5 + 4 + (3 * num_explicit_lens as u64);
    for (sym, (&freq, &len)) in precode_freqs.iter().zip(precode_lens.iter()).enumerate() {
        let extra = EXTRA_PRECODE_BITS[sym] as u64;
        dynamic_cost += freq as u64 * (extra + len as u64);
    }

    // Literal cost
    for sym in 0..144usize {
        dynamic_cost += freqs.litlen[sym] as u64 * codes.lens_litlen[sym] as u64;
        static_cost += freqs.litlen[sym] as u64 * 8;
    }
    for sym in 144..256usize {
        dynamic_cost += freqs.litlen[sym] as u64 * codes.lens_litlen[sym] as u64;
        static_cost += freqs.litlen[sym] as u64 * 9;
    }

    // End-of-block cost
    dynamic_cost += codes.lens_litlen[DEFLATE_END_OF_BLOCK as usize] as u64;
    static_cost += 7;

    // Length symbol cost
    for (i, &extra_bits) in DEFLATE_LENGTH_EXTRA_BITS.iter().enumerate() {
        let sym = DEFLATE_FIRST_LEN_SYM as usize + i;
        let extra = extra_bits as u64;
        dynamic_cost += freqs.litlen[sym] as u64 * (extra + codes.lens_litlen[sym] as u64);
        static_cost += freqs.litlen[sym] as u64 * (extra + static_codes.lens_litlen[sym] as u64);
    }

    // Offset symbol cost
    for (sym, &extra_bits) in DEFLATE_OFFSET_EXTRA_BITS[..30].iter().enumerate() {
        let extra = extra_bits as u64;
        dynamic_cost += freqs.offset[sym] as u64 * (extra + codes.lens_offset[sym] as u64);
        static_cost += freqs.offset[sym] as u64 * (extra + 5);
    }

    // Uncompressed cost
    let align_bits = (u64::MAX - (bitcount as u64 + 3) + 1) & 7;
    let num_full_blocks = block_length.saturating_sub(1) / 0xFFFF;
    let uncompressed_cost =
        align_bits + 32 + (40 * num_full_blocks as u64) + (8 * block_length as u64);

    // ---- Choose cheapest block type ----
    let best_cost = dynamic_cost.min(static_cost).min(uncompressed_cost);

    // Check if block fits
    let bytes_needed = (bitcount as u64 + best_cost).div_ceil(8);
    if bytes_needed > os.remaining() as u64 {
        os.overflow = true;
        return;
    }

    if best_cost == uncompressed_cost {
        // Write uncompressed block(s)
        write_uncompressed_blocks(os, in_data, is_final_block);
        return;
    }

    let use_static = best_cost == static_cost;
    let active_codes = if use_static { static_codes } else { codes };

    if use_static {
        // Static Huffman block header
        os.add_bits(is_final_block as u32, 1);
        os.add_bits(DEFLATE_BLOCKTYPE_STATIC_HUFFMAN, 2);
        os.flush_bits();
    } else {
        // Dynamic Huffman block header
        // CAN_BUFFER(1 + 2 + 5 + 5 + 4 + 3) = 7 + 20 = 27 ≤ 63 ✓
        os.add_bits(is_final_block as u32, 1);
        os.add_bits(DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN, 2);
        os.add_bits(num_litlen_syms as u32 - 257, 5);
        os.add_bits(num_offset_syms as u32 - 1, 5);
        os.add_bits(num_explicit_lens as u32 - 4, 4);

        // Output precode lengths.
        // A 64-bit bitbuffer is one bit too small for all 19 precode lengths
        // (19×3=57, and 7+57=64 > 63=BITBUF_NBITS), so merge the first
        // precode length with the header before flushing, matching libdeflate.
        const _: () = assert!(can_buffer(1 + 2 + 5 + 5 + 4 + 3));
        const _: () = assert!(can_buffer(3 * (DEFLATE_NUM_PRECODE_SYMS - 1)));
        let first_perm = DEFLATE_PRECODE_LENS_PERMUTATION[0];
        os.add_bits(precode_lens[first_perm as usize] as u32, 3);
        os.flush_bits();

        // Remaining precode lengths: up to 18×3=54 bits, 7+54=61 ≤ 63 ✓
        for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION[1..num_explicit_lens] {
            os.add_bits(precode_lens[perm as usize] as u32, 3);
        }
        os.flush_bits();

        // Output precode items (encoded code lengths)
        for &item in &precode_items[..num_precode_items] {
            let sym = (item & 0x1F) as usize;
            os.add_bits(precode_codewords[sym], precode_lens[sym] as u32);
            os.add_bits(item >> 5, EXTRA_PRECODE_BITS[sym] as u32);
            os.flush_bits();
        }
    }

    // ---- Compute full length codewords ----
    let mut full_len_codewords = [0u32; DEFLATE_MAX_MATCH_LEN as usize + 1];
    let mut full_len_lens = [0u8; DEFLATE_MAX_MATCH_LEN as usize + 1];
    for len in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
        let slot = LENGTH_SLOT[len as usize] as usize;
        let litlen_sym = DEFLATE_FIRST_LEN_SYM as usize + slot;
        let extra_bits = len - DEFLATE_LENGTH_BASE[slot] as u32;
        full_len_codewords[len as usize] = active_codes.codewords_litlen[litlen_sym]
            | (extra_bits << active_codes.lens_litlen[litlen_sym]);
        full_len_lens[len as usize] =
            active_codes.lens_litlen[litlen_sym] + DEFLATE_LENGTH_EXTRA_BITS[slot];
    }

    // ---- Output literals and matches ----
    //
    // Use local bitbuf/bitcount to avoid aliasing-induced stores on every add_bits.
    // The C code uses local variables via ADD_BITS/FLUSH_BITS macros for the same reason.
    //
    // Compile-time capacity checks (matching libdeflate's CAN_BUFFER):
    //   4 literals:    7 + 4*14 = 63 ≤ 63  ✓
    //   full match:    7 + 14+5+15+13 = 54 ≤ 63  ✓
    const _: () = assert!(7 + 4 * MAX_LITLEN_CODEWORD_LEN <= BITBUF_NBITS);
    const _: () = assert!(
        7 + MAX_LITLEN_CODEWORD_LEN
            + DEFLATE_MAX_EXTRA_LENGTH_BITS
            + DEFLATE_MAX_OFFSET_CODEWORD_LEN
            + DEFLATE_MAX_EXTRA_OFFSET_BITS
            <= BITBUF_NBITS
    );

    let mut bitbuf = os.bitbuf;
    let mut bitcount = os.bitcount;

    // Local add_bits: accumulate into register-resident locals
    macro_rules! add_bits {
        ($bits:expr, $n:expr) => {{
            bitbuf |= ($bits as u64) << bitcount;
            bitcount += $n;
        }};
    }

    // Local flush_bits: write through os.buf, keep bitbuf/bitcount local
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

    match output {
        BlockOutput::Sequences(sequences) => {
            let mut in_pos = 0usize;
            for seq in sequences {
                let mut litrunlen = seq.litrunlen();
                let length = seq.length();

                // Output literal run — batch 4 per flush
                while litrunlen >= 4 {
                    let lit0 = crate::fast_bytes::get_byte(in_data, in_pos) as usize;
                    let lit1 = crate::fast_bytes::get_byte(in_data, in_pos + 1) as usize;
                    let lit2 = crate::fast_bytes::get_byte(in_data, in_pos + 2) as usize;
                    let lit3 = crate::fast_bytes::get_byte(in_data, in_pos + 3) as usize;
                    add_bits!(
                        active_codes.codewords_litlen[lit0],
                        active_codes.lens_litlen[lit0] as u32
                    );
                    add_bits!(
                        active_codes.codewords_litlen[lit1],
                        active_codes.lens_litlen[lit1] as u32
                    );
                    add_bits!(
                        active_codes.codewords_litlen[lit2],
                        active_codes.lens_litlen[lit2] as u32
                    );
                    add_bits!(
                        active_codes.codewords_litlen[lit3],
                        active_codes.lens_litlen[lit3] as u32
                    );
                    flush_bits!();
                    in_pos += 4;
                    litrunlen -= 4;
                }
                // Remainder (0..3 literals)
                if litrunlen > 0 {
                    let lit = crate::fast_bytes::get_byte(in_data, in_pos) as usize;
                    in_pos += 1;
                    add_bits!(
                        active_codes.codewords_litlen[lit],
                        active_codes.lens_litlen[lit] as u32
                    );
                    if litrunlen > 1 {
                        let lit = crate::fast_bytes::get_byte(in_data, in_pos) as usize;
                        in_pos += 1;
                        add_bits!(
                            active_codes.codewords_litlen[lit],
                            active_codes.lens_litlen[lit] as u32
                        );
                        if litrunlen > 2 {
                            let lit = crate::fast_bytes::get_byte(in_data, in_pos) as usize;
                            in_pos += 1;
                            add_bits!(
                                active_codes.codewords_litlen[lit],
                                active_codes.lens_litlen[lit] as u32
                            );
                        }
                    }
                    flush_bits!();
                }

                if length == 0 {
                    break;
                }

                // Output match — single flush for all bits
                let offset_slot = seq.offset_slot as usize;
                add_bits!(
                    full_len_codewords[length as usize],
                    full_len_lens[length as usize] as u32
                );
                add_bits!(
                    active_codes.codewords_offset[offset_slot],
                    active_codes.lens_offset[offset_slot] as u32
                );
                add_bits!(
                    seq.offset as u32 - DEFLATE_OFFSET_BASE[offset_slot],
                    DEFLATE_OFFSET_EXTRA_BITS[offset_slot] as u32
                );
                flush_bits!();

                in_pos += length as usize;
            }
        }
        BlockOutput::Optimum {
            nodes,
            block_length: bl,
            offset_slot_full,
        } => {
            let mut cur_idx = 0;
            while cur_idx < bl {
                let item = nodes[cur_idx].item;
                let length = item & OPTIMUM_LEN_MASK;
                let offset = item >> OPTIMUM_OFFSET_SHIFT;

                if length == 1 {
                    // Literal
                    let lit = offset as usize;
                    add_bits!(
                        active_codes.codewords_litlen[lit],
                        active_codes.lens_litlen[lit] as u32
                    );
                    flush_bits!();
                } else {
                    // Match — single flush for all bits
                    let os_idx = offset_slot_full[offset as usize] as usize;
                    add_bits!(
                        full_len_codewords[length as usize],
                        full_len_lens[length as usize] as u32
                    );
                    add_bits!(
                        active_codes.codewords_offset[os_idx],
                        active_codes.lens_offset[os_idx] as u32
                    );
                    add_bits!(
                        offset - DEFLATE_OFFSET_BASE[os_idx],
                        DEFLATE_OFFSET_EXTRA_BITS[os_idx] as u32
                    );
                    flush_bits!();
                }
                cur_idx += length as usize;
            }
        }
    }

    // Output end-of-block symbol
    add_bits!(
        active_codes.codewords_litlen[DEFLATE_END_OF_BLOCK as usize],
        active_codes.lens_litlen[DEFLATE_END_OF_BLOCK as usize] as u32
    );
    flush_bits!();

    // Sync local state back to output bitstream
    os.bitbuf = bitbuf;
    os.bitcount = bitcount;
}

/// Write uncompressed block(s), splitting at UINT16_MAX boundaries.
fn write_uncompressed_blocks(os: &mut OutputBitstream<'_>, data: &[u8], is_final_block: bool) {
    let mut remaining = data;

    while !remaining.is_empty() {
        let is_last = remaining.len() <= 0xFFFF;
        let len = remaining.len().min(0xFFFF);
        let chunk = &remaining[..len];
        remaining = &remaining[len..];

        let bfinal = if is_last && is_final_block { 1u8 } else { 0 };

        // Write BFINAL + BTYPE (uncompressed = 0), then align to byte boundary
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

/// Record a literal into the frequency table and current sequence.
#[inline(always)]
pub(crate) fn choose_literal(freqs: &mut DeflateFreqs, literal: u8, seq: &mut Sequence) {
    freqs.litlen[literal as usize] += 1;
    seq.litrunlen_and_length += 1;
}

/// Record a match into the frequency table and advance the sequence pointer.
///
/// Returns the index of the new (next) sequence.
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn choose_match(
    freqs: &mut DeflateFreqs,
    length: u32,
    offset: u32,
    sequences: &mut [Sequence],
    seq_idx: usize,
) -> usize {
    let length_slot = LENGTH_SLOT[length as usize];
    let offset_slot = get_offset_slot(offset);

    freqs.litlen[DEFLATE_FIRST_LEN_SYM as usize + length_slot as usize] += 1;
    freqs.offset[offset_slot as usize] += 1;

    sequences[seq_idx].litrunlen_and_length |= length << super::sequences::SEQ_LENGTH_SHIFT;
    sequences[seq_idx].offset = offset as u16;
    sequences[seq_idx].offset_slot = offset_slot as u16;

    let next = seq_idx + 1;
    sequences[next].litrunlen_and_length = 0;
    next
}

/// Build codes and flush a finished block (adds end-of-block symbol first).
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_block(
    os: &mut OutputBitstream<'_>,
    block_begin: &[u8],
    block_length: usize,
    sequences: &[Sequence],
    freqs: &mut DeflateFreqs,
    codes: &mut DeflateCodes,
    static_codes: &DeflateCodes,
    is_final_block: bool,
) {
    freqs.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;
    make_huffman_codes(freqs, codes);
    flush_block(
        os,
        block_begin,
        block_length,
        BlockOutput::Sequences(sequences),
        freqs,
        codes,
        static_codes,
        is_final_block,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_offset_slot() {
        assert_eq!(get_offset_slot(1), 0);
        assert_eq!(get_offset_slot(2), 1);
        assert_eq!(get_offset_slot(3), 2);
        assert_eq!(get_offset_slot(4), 3);
        assert_eq!(get_offset_slot(5), 4);
        assert_eq!(get_offset_slot(7), 5);
        assert_eq!(get_offset_slot(9), 6);
        assert_eq!(get_offset_slot(13), 7);
        assert_eq!(get_offset_slot(256), 15);
        assert_eq!(get_offset_slot(257), 16);
        assert_eq!(get_offset_slot(32768), 29);
    }

    #[test]
    fn test_length_slot() {
        assert_eq!(LENGTH_SLOT[3], 0); // min match
        assert_eq!(LENGTH_SLOT[4], 1);
        assert_eq!(LENGTH_SLOT[10], 7);
        assert_eq!(LENGTH_SLOT[258], 28); // max match
    }

    #[test]
    fn test_static_codes_valid() {
        let mut freqs = DeflateFreqs::default();
        let mut codes = DeflateCodes::default();
        init_static_codes(&mut freqs, &mut codes);

        // Static codes: 0-143 = 8 bits, 144-255 = 9 bits, 256-279 = 7 bits, 280-287 = 8 bits
        assert_eq!(codes.lens_litlen[0], 8);
        assert_eq!(codes.lens_litlen[143], 8);
        assert_eq!(codes.lens_litlen[144], 9);
        assert_eq!(codes.lens_litlen[255], 9);
        assert_eq!(codes.lens_litlen[256], 7); // end-of-block
        assert_eq!(codes.lens_litlen[279], 7);
        assert_eq!(codes.lens_litlen[280], 8);
        assert_eq!(codes.lens_litlen[287], 8);
    }
}

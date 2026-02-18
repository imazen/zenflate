//! Block flushing: choose best block type and encode it.
//!
//! Ported from libdeflate's `deflate_flush_block()`, `deflate_precompute_huffman_header()`,
//! `deflate_compute_precode_items()`, `deflate_compute_full_len_codewords()`.

use crate::constants::*;

use super::bitstream::{BITBUF_NBITS, OutputBitstream};
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

/// Compute RLE-encoded precode items for the combined lens array.
///
/// Returns the number of items written to `precode_items`.
pub(crate) fn compute_precode_items(
    lens: &[u8],
    precode_freqs: &mut [u32; DEFLATE_NUM_PRECODE_SYMS as usize],
    precode_items: &mut [u32],
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
            while run_end - run_start >= 11 {
                let extra_bits = (run_end - run_start - 11).min(0x7F) as u32;
                precode_freqs[18] += 1;
                precode_items[item_count] = 18 | (extra_bits << 5);
                item_count += 1;
                run_start += 11 + extra_bits as usize;
            }
            // Symbol 17: RLE 3..=10 zeroes
            if run_end - run_start >= 3 {
                let extra_bits = (run_end - run_start - 3).min(0x7) as u32;
                precode_freqs[17] += 1;
                precode_items[item_count] = 17 | (extra_bits << 5);
                item_count += 1;
                run_start += 3 + extra_bits as usize;
            }
        } else {
            // Run of nonzero lengths
            // Symbol 16: RLE 3..=6 of previous length
            if run_end - run_start >= 4 {
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

    // Compute precode items (RLE tokens)
    let mut precode_freqs = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_items = [0u32; (DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS) as usize];
    let num_precode_items = compute_precode_items(
        &combined_lens[..total_lens],
        &mut precode_freqs,
        &mut precode_items,
    );

    // Build precode Huffman code
    let mut precode_lens = [0u8; DEFLATE_NUM_PRECODE_SYMS as usize];
    let mut precode_codewords = [0u32; DEFLATE_NUM_PRECODE_SYMS as usize];
    make_huffman_code(
        DEFLATE_NUM_PRECODE_SYMS as usize,
        DEFLATE_MAX_PRE_CODEWORD_LEN,
        &precode_freqs,
        &mut precode_lens,
        &mut precode_codewords,
    );

    // Count how many precode lengths to output
    let mut num_explicit_lens = DEFLATE_NUM_PRECODE_SYMS as usize;
    while num_explicit_lens > 4
        && precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[num_explicit_lens - 1] as usize] == 0
    {
        num_explicit_lens -= 1;
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
        os.add_bits(is_final_block as u32, 1);
        os.add_bits(DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN, 2);
        os.add_bits(num_litlen_syms as u32 - 257, 5);
        os.add_bits(num_offset_syms as u32 - 1, 5);
        os.add_bits(num_explicit_lens as u32 - 4, 4);
        os.flush_bits();

        // Output precode lengths
        for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION[..num_explicit_lens] {
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

    match output {
        BlockOutput::Sequences(sequences) => {
            let mut in_pos = 0usize;
            for seq in sequences {
                let mut litrunlen = seq.litrunlen();
                let length = seq.length();

                // Output literal run — batch 4 per flush
                while litrunlen >= 4 {
                    let lit0 = in_data[in_pos] as usize;
                    let lit1 = in_data[in_pos + 1] as usize;
                    let lit2 = in_data[in_pos + 2] as usize;
                    let lit3 = in_data[in_pos + 3] as usize;
                    os.add_bits(
                        active_codes.codewords_litlen[lit0],
                        active_codes.lens_litlen[lit0] as u32,
                    );
                    os.add_bits(
                        active_codes.codewords_litlen[lit1],
                        active_codes.lens_litlen[lit1] as u32,
                    );
                    os.add_bits(
                        active_codes.codewords_litlen[lit2],
                        active_codes.lens_litlen[lit2] as u32,
                    );
                    os.add_bits(
                        active_codes.codewords_litlen[lit3],
                        active_codes.lens_litlen[lit3] as u32,
                    );
                    os.flush_bits();
                    in_pos += 4;
                    litrunlen -= 4;
                }
                // Remainder (0..3 literals)
                if litrunlen > 0 {
                    let lit = in_data[in_pos] as usize;
                    in_pos += 1;
                    os.add_bits(
                        active_codes.codewords_litlen[lit],
                        active_codes.lens_litlen[lit] as u32,
                    );
                    if litrunlen > 1 {
                        let lit = in_data[in_pos] as usize;
                        in_pos += 1;
                        os.add_bits(
                            active_codes.codewords_litlen[lit],
                            active_codes.lens_litlen[lit] as u32,
                        );
                        if litrunlen > 2 {
                            let lit = in_data[in_pos] as usize;
                            in_pos += 1;
                            os.add_bits(
                                active_codes.codewords_litlen[lit],
                                active_codes.lens_litlen[lit] as u32,
                            );
                        }
                    }
                    os.flush_bits();
                }

                if length == 0 {
                    break;
                }

                // Output match — single flush for all bits
                let offset_slot = seq.offset_slot as usize;
                os.add_bits(
                    full_len_codewords[length as usize],
                    full_len_lens[length as usize] as u32,
                );
                os.add_bits(
                    active_codes.codewords_offset[offset_slot],
                    active_codes.lens_offset[offset_slot] as u32,
                );
                os.add_bits(
                    seq.offset as u32 - DEFLATE_OFFSET_BASE[offset_slot],
                    DEFLATE_OFFSET_EXTRA_BITS[offset_slot] as u32,
                );
                os.flush_bits();

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
                    os.add_bits(
                        active_codes.codewords_litlen[lit],
                        active_codes.lens_litlen[lit] as u32,
                    );
                    os.flush_bits();
                } else {
                    // Match — single flush for all bits
                    let os_idx = offset_slot_full[offset as usize] as usize;
                    os.add_bits(
                        full_len_codewords[length as usize],
                        full_len_lens[length as usize] as u32,
                    );
                    os.add_bits(
                        active_codes.codewords_offset[os_idx],
                        active_codes.lens_offset[os_idx] as u32,
                    );
                    os.add_bits(
                        offset - DEFLATE_OFFSET_BASE[os_idx],
                        DEFLATE_OFFSET_EXTRA_BITS[os_idx] as u32,
                    );
                    os.flush_bits();
                }
                cur_idx += length as usize;
            }
        }
    }

    // Output end-of-block symbol
    os.add_bits(
        active_codes.codewords_litlen[DEFLATE_END_OF_BLOCK as usize],
        active_codes.lens_litlen[DEFLATE_END_OF_BLOCK as usize] as u32,
    );
    os.flush_bits();
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

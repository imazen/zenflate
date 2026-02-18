//! Huffman code construction for DEFLATE compression.
//!
//! Ported from libdeflate's `deflate_compress.c`:
//! - `sort_symbols()`: hybrid count-sort + heapsort
//! - `build_tree()`: in-place Huffman tree from sorted frequencies
//! - `compute_length_counts()`: length-limited tree traversal
//! - `gen_codewords()`: canonical Huffman codeword generation with bit-reversal
//! - `deflate_make_huffman_code()`: main entry point

use crate::constants::{DEFLATE_MAX_CODEWORD_LEN, DEFLATE_MAX_NUM_SYMS};

/// Low 10 bits hold the symbol value.
const NUM_SYMBOL_BITS: u32 = 10;
/// High 22 bits hold the frequency.
const SYMBOL_MASK: u32 = (1 << NUM_SYMBOL_BITS) - 1;
const FREQ_MASK: u32 = !SYMBOL_MASK;

/// Bit-reversal lookup table for 8-bit values.
/// Used to produce DEFLATE's bit-reversed codewords.
#[rustfmt::skip]
const BITREVERSE_TAB: [u8; 256] = [
    0x00, 0x80, 0x40, 0xc0, 0x20, 0xa0, 0x60, 0xe0,
    0x10, 0x90, 0x50, 0xd0, 0x30, 0xb0, 0x70, 0xf0,
    0x08, 0x88, 0x48, 0xc8, 0x28, 0xa8, 0x68, 0xe8,
    0x18, 0x98, 0x58, 0xd8, 0x38, 0xb8, 0x78, 0xf8,
    0x04, 0x84, 0x44, 0xc4, 0x24, 0xa4, 0x64, 0xe4,
    0x14, 0x94, 0x54, 0xd4, 0x34, 0xb4, 0x74, 0xf4,
    0x0c, 0x8c, 0x4c, 0xcc, 0x2c, 0xac, 0x6c, 0xec,
    0x1c, 0x9c, 0x5c, 0xdc, 0x3c, 0xbc, 0x7c, 0xfc,
    0x02, 0x82, 0x42, 0xc2, 0x22, 0xa2, 0x62, 0xe2,
    0x12, 0x92, 0x52, 0xd2, 0x32, 0xb2, 0x72, 0xf2,
    0x0a, 0x8a, 0x4a, 0xca, 0x2a, 0xaa, 0x6a, 0xea,
    0x1a, 0x9a, 0x5a, 0xda, 0x3a, 0xba, 0x7a, 0xfa,
    0x06, 0x86, 0x46, 0xc6, 0x26, 0xa6, 0x66, 0xe6,
    0x16, 0x96, 0x56, 0xd6, 0x36, 0xb6, 0x76, 0xf6,
    0x0e, 0x8e, 0x4e, 0xce, 0x2e, 0xae, 0x6e, 0xee,
    0x1e, 0x9e, 0x5e, 0xde, 0x3e, 0xbe, 0x7e, 0xfe,
    0x01, 0x81, 0x41, 0xc1, 0x21, 0xa1, 0x61, 0xe1,
    0x11, 0x91, 0x51, 0xd1, 0x31, 0xb1, 0x71, 0xf1,
    0x09, 0x89, 0x49, 0xc9, 0x29, 0xa9, 0x69, 0xe9,
    0x19, 0x99, 0x59, 0xd9, 0x39, 0xb9, 0x79, 0xf9,
    0x05, 0x85, 0x45, 0xc5, 0x25, 0xa5, 0x65, 0xe5,
    0x15, 0x95, 0x55, 0xd5, 0x35, 0xb5, 0x75, 0xf5,
    0x0d, 0x8d, 0x4d, 0xcd, 0x2d, 0xad, 0x6d, 0xed,
    0x1d, 0x9d, 0x5d, 0xdd, 0x3d, 0xbd, 0x7d, 0xfd,
    0x03, 0x83, 0x43, 0xc3, 0x23, 0xa3, 0x63, 0xe3,
    0x13, 0x93, 0x53, 0xd3, 0x33, 0xb3, 0x73, 0xf3,
    0x0b, 0x8b, 0x4b, 0xcb, 0x2b, 0xab, 0x6b, 0xeb,
    0x1b, 0x9b, 0x5b, 0xdb, 0x3b, 0xbb, 0x7b, 0xfb,
    0x07, 0x87, 0x47, 0xc7, 0x27, 0xa7, 0x67, 0xe7,
    0x17, 0x97, 0x57, 0xd7, 0x37, 0xb7, 0x77, 0xf7,
    0x0f, 0x8f, 0x4f, 0xcf, 0x2f, 0xaf, 0x6f, 0xef,
    0x1f, 0x9f, 0x5f, 0xdf, 0x3f, 0xbf, 0x7f, 0xff,
];

/// Reverse a codeword of `len` bits.
#[inline]
fn reverse_codeword(codeword: u32, len: u8) -> u32 {
    debug_assert!(len <= 16);
    let reversed = ((BITREVERSE_TAB[(codeword & 0xFF) as usize] as u32) << 8)
        | (BITREVERSE_TAB[(codeword >> 8) as usize] as u32);
    reversed >> (16 - len)
}

// ---- Heapsort for high-frequency symbols ----

/// Sift down for maxheap. `a` uses 1-based indexing.
#[allow(dead_code)]
fn heapify_subtree(a: &mut [u32], length: usize, subtree_idx: usize) {
    let v = a[subtree_idx];
    let mut parent_idx = subtree_idx;
    loop {
        let mut child_idx = parent_idx * 2;
        if child_idx > length {
            break;
        }
        if child_idx < length && a[child_idx + 1] > a[child_idx] {
            child_idx += 1;
        }
        if v >= a[child_idx] {
            break;
        }
        a[parent_idx] = a[child_idx];
        parent_idx = child_idx;
    }
    a[parent_idx] = v;
}

/// Build a maxheap. `a` uses 1-based indexing.
#[allow(dead_code)]
fn heapify_array(a: &mut [u32], length: usize) {
    for subtree_idx in (1..=length / 2).rev() {
        heapify_subtree(a, length, subtree_idx);
    }
}

/// Heapsort an array of u32 values. The input slice is 0-based.
fn heap_sort(a: &mut [u32]) {
    let len = a.len();
    if len < 2 {
        return;
    }
    // Shift to 1-based by working with a[1..] after prepending conceptually.
    // We use the trick from libdeflate: `A--` to shift to 1-based.
    // In Rust, we work around this by creating a wrapper that indexes from a[-1].
    // Actually, we can just shift the slice: operate on indices [0..len) as [1..len+1).
    // We need 1-based access, so we'll work with a helper.

    // Build maxheap (1-based, so element at index 0 is "index 1")
    // We need to offset. Let's just do it with explicit offset math.
    heapify_array_0based(a, len);

    let mut length = len;
    while length >= 2 {
        a.swap(0, length - 1);
        length -= 1;
        heapify_subtree_0based(a, length, 0);
    }
}

/// Sift down for maxheap, 0-based indexing.
fn heapify_subtree_0based(a: &mut [u32], length: usize, root: usize) {
    let v = a[root];
    let mut parent = root;
    loop {
        let mut child = parent * 2 + 1;
        if child >= length {
            break;
        }
        if child + 1 < length && a[child + 1] > a[child] {
            child += 1;
        }
        if v >= a[child] {
            break;
        }
        a[parent] = a[child];
        parent = child;
    }
    a[parent] = v;
}

/// Build maxheap, 0-based indexing.
fn heapify_array_0based(a: &mut [u32], length: usize) {
    if length < 2 {
        return;
    }
    for i in (0..length / 2).rev() {
        heapify_subtree_0based(a, length, i);
    }
}

/// Sort symbols by frequency, discarding zero-frequency symbols.
///
/// Returns the number of symbols with nonzero frequency.
/// Fills `symout` with packed (freq << NUM_SYMBOL_BITS | symbol) values, sorted.
/// Sets lens[sym] = 0 for zero-frequency symbols.
fn sort_symbols(num_syms: usize, freqs: &[u32], lens: &mut [u8], symout: &mut [u32]) -> usize {
    let num_counters = num_syms;
    let mut counters = [0u32; DEFLATE_MAX_NUM_SYMS as usize];

    // Count frequencies into buckets.
    for &freq in &freqs[..num_syms] {
        let bucket = (freq as usize).min(num_counters - 1);
        counters[bucket] += 1;
    }

    // Make counters cumulative, skipping zero-frequency bucket.
    let mut num_used_syms = 0usize;
    for counter in &mut counters[1..num_counters] {
        let count = *counter;
        *counter = num_used_syms as u32;
        num_used_syms += count as usize;
    }

    // Place symbols into sorted positions. Zero-frequency symbols get lens=0.
    for sym in 0..num_syms {
        let freq = freqs[sym];
        if freq != 0 {
            let bucket = (freq as usize).min(num_counters - 1);
            let idx = counters[bucket] as usize;
            symout[idx] = (sym as u32) | (freq << NUM_SYMBOL_BITS);
            counters[bucket] += 1;
        } else {
            lens[sym] = 0;
        }
    }

    // Heapsort the symbols in the last counter bucket (high frequencies).
    if num_counters >= 2 {
        let start = counters[num_counters - 2] as usize;
        let end = counters[num_counters - 1] as usize;
        if end > start {
            heap_sort(&mut symout[start..end]);
        }
    }

    num_used_syms
}

/// Build a stripped-down Huffman tree in-place.
///
/// Input: `a[0..sym_count]` contains sorted (freq << 10 | symbol) entries.
/// Output: `a[0..sym_count-1]` contains non-leaf nodes with parent indices in high bits.
/// The root is at `a[sym_count - 2]`.
fn build_tree(a: &mut [u32], sym_count: usize) {
    debug_assert!(sym_count >= 2);
    let last_idx = sym_count - 1;

    let mut i = 0usize; // next leaf
    let mut b = 0usize; // next non-leaf
    let mut e = 0usize; // next output slot

    loop {
        let new_freq;

        // Select two lowest frequency nodes and create a new parent.
        if i < last_idx && (b == e || (a[i + 1] & FREQ_MASK) <= (a[b] & FREQ_MASK)) {
            // Two leaves
            new_freq = (a[i] & FREQ_MASK).wrapping_add(a[i + 1] & FREQ_MASK);
            i += 2;
        } else if b + 2 <= e && (i > last_idx || (a[b + 1] & FREQ_MASK) < (a[i] & FREQ_MASK)) {
            // Two non-leaves
            new_freq = (a[b] & FREQ_MASK).wrapping_add(a[b + 1] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            a[b + 1] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b + 1] & SYMBOL_MASK);
            b += 2;
        } else {
            // One leaf and one non-leaf
            new_freq = (a[i] & FREQ_MASK).wrapping_add(a[b] & FREQ_MASK);
            a[b] = ((e as u32) << NUM_SYMBOL_BITS) | (a[b] & SYMBOL_MASK);
            i += 1;
            b += 1;
        }
        a[e] = new_freq | (a[e] & SYMBOL_MASK);

        e += 1;
        if e >= last_idx {
            break;
        }
    }
}

/// Compute the number of codewords for each length, with length-limiting.
///
/// `a[0..root_idx+1]` contains the Huffman tree non-leaf nodes (from `build_tree`).
/// `root_idx` is `sym_count - 2`.
/// Fills `len_counts[0..=max_codeword_len]`.
fn compute_length_counts(
    a: &mut [u32],
    root_idx: usize,
    len_counts: &mut [u32],
    max_codeword_len: u32,
) {
    let max_len = max_codeword_len as usize;

    for c in len_counts[..=max_len].iter_mut() {
        *c = 0;
    }
    len_counts[1] = 2;

    // Set root depth to 0.
    a[root_idx] &= SYMBOL_MASK;

    // Traverse non-leaf nodes in reverse order (parent before children).
    for node in (0..root_idx).rev() {
        let parent = (a[node] >> NUM_SYMBOL_BITS) as usize;
        let parent_depth = (a[parent] >> NUM_SYMBOL_BITS) as usize;
        let mut depth = parent_depth + 1;

        // Store depth for children to use.
        a[node] = (a[node] & SYMBOL_MASK) | ((depth as u32) << NUM_SYMBOL_BITS);

        // Length-limit: clamp to max_codeword_len.
        if depth >= max_len {
            depth = max_len;
            loop {
                depth -= 1;
                if len_counts[depth] != 0 {
                    break;
                }
            }
        }

        // Account for this non-leaf: lose one codeword at depth, gain two at depth+1.
        len_counts[depth] -= 1;
        len_counts[depth + 1] += 2;
    }
}

/// Generate canonical Huffman codewords (bit-reversed for DEFLATE).
///
/// `a[0..num_used_syms]` contains sorted symbols in low bits.
/// `lens[0..num_syms]` is filled with codeword lengths.
/// `a[0..num_syms]` (= `codewords`) is filled with bit-reversed codewords.
fn gen_codewords(
    a: &mut [u32],
    lens: &mut [u8],
    len_counts: &[u32],
    max_codeword_len: u32,
    num_syms: usize,
) {
    let max_len = max_codeword_len as usize;

    // Assign lengths in decreasing order to symbols sorted by increasing frequency.
    let mut i = 0usize;
    for len in (1..=max_len).rev() {
        let mut count = len_counts[len];
        while count > 0 {
            lens[(a[i] & SYMBOL_MASK) as usize] = len as u8;
            i += 1;
            count -= 1;
        }
    }

    // Generate canonical codewords.
    let mut next_codewords = [0u32; DEFLATE_MAX_CODEWORD_LEN as usize + 1];
    for len in 2..=max_len {
        next_codewords[len] = (next_codewords[len - 1] + len_counts[len - 1]) << 1;
    }

    for sym in 0..num_syms {
        let len = lens[sym];
        if len > 0 {
            a[sym] = reverse_codeword(next_codewords[len as usize], len);
            next_codewords[len as usize] += 1;
        }
    }
}

/// Build a length-limited canonical Huffman code.
///
/// Given symbol frequencies, produces codewords and codeword lengths.
/// Handles edge cases: 0 or 1 used symbols produce a 2-symbol code.
pub(crate) fn make_huffman_code(
    num_syms: usize,
    max_codeword_len: u32,
    freqs: &[u32],
    lens: &mut [u8],
    codewords: &mut [u32],
) {
    let a = codewords;

    let num_used_syms = sort_symbols(num_syms, freqs, lens, a);

    // Need at least 2 codewords for a complete Huffman code.
    if num_used_syms < 2 {
        let sym = if num_used_syms > 0 {
            (a[0] & SYMBOL_MASK) as usize
        } else {
            0
        };
        let nonzero_idx = if sym != 0 { sym } else { 1 };

        // Zero everything first
        for cw in a[..num_syms].iter_mut() {
            *cw = 0;
        }
        for l in lens[..num_syms].iter_mut() {
            *l = 0;
        }

        a[0] = 0;
        lens[0] = 1;
        a[nonzero_idx] = 1;
        lens[nonzero_idx] = 1;
        return;
    }

    build_tree(a, num_used_syms);

    let root_idx = num_used_syms - 2;
    let mut len_counts = [0u32; DEFLATE_MAX_CODEWORD_LEN as usize + 1];
    compute_length_counts(a, root_idx, &mut len_counts, max_codeword_len);
    gen_codewords(a, lens, &len_counts, max_codeword_len, num_syms);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_codeword() {
        assert_eq!(reverse_codeword(0b0, 1), 0b0);
        assert_eq!(reverse_codeword(0b1, 1), 0b1);
        assert_eq!(reverse_codeword(0b10, 2), 0b01);
        assert_eq!(reverse_codeword(0b11, 2), 0b11);
        assert_eq!(reverse_codeword(0b1010, 4), 0b0101);
        assert_eq!(reverse_codeword(0b110, 3), 0b011);
    }

    #[test]
    fn test_heap_sort_empty() {
        let mut a: [u32; 0] = [];
        heap_sort(&mut a);
    }

    #[test]
    fn test_heap_sort_sorted() {
        let mut a = [1u32, 2, 3, 4, 5];
        heap_sort(&mut a);
        assert_eq!(a, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_heap_sort_reverse() {
        let mut a = [5u32, 4, 3, 2, 1];
        heap_sort(&mut a);
        assert_eq!(a, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_heap_sort_duplicates() {
        let mut a = [3u32, 1, 3, 1, 2];
        heap_sort(&mut a);
        assert_eq!(a, [1, 1, 2, 3, 3]);
    }

    #[test]
    fn test_make_huffman_code_simple() {
        // Two symbols with equal frequency
        let freqs = [10u32, 10, 0, 0];
        let mut lens = [0u8; 4];
        let mut codewords = [0u32; 4];
        make_huffman_code(4, 15, &freqs, &mut lens, &mut codewords);

        assert_eq!(lens[0], 1);
        assert_eq!(lens[1], 1);
        assert_eq!(lens[2], 0);
        assert_eq!(lens[3], 0);
        // One should be 0 and one should be 1
        assert_ne!(codewords[0], codewords[1]);
    }

    #[test]
    fn test_make_huffman_code_skewed() {
        // Heavily skewed frequencies: sym 0 very common, others rare
        let freqs = [100u32, 1, 1, 1];
        let mut lens = [0u8; 4];
        let mut codewords = [0u32; 4];
        make_huffman_code(4, 15, &freqs, &mut lens, &mut codewords);

        // Symbol 0 should have the shortest code
        assert!(lens[0] <= lens[1]);
        assert!(lens[0] <= lens[2]);
        assert!(lens[0] <= lens[3]);
    }

    #[test]
    fn test_make_huffman_code_single_symbol() {
        let freqs = [10u32, 0, 0, 0];
        let mut lens = [0u8; 4];
        let mut codewords = [0u32; 4];
        make_huffman_code(4, 15, &freqs, &mut lens, &mut codewords);

        // Should produce a valid 2-symbol code
        assert_eq!(lens[0], 1);
        assert_eq!(lens[1], 1);
        assert_eq!(codewords[0], 0);
        assert_eq!(codewords[1], 1);
    }

    #[test]
    fn test_make_huffman_code_no_symbols() {
        let freqs = [0u32; 4];
        let mut lens = [0u8; 4];
        let mut codewords = [0u32; 4];
        make_huffman_code(4, 15, &freqs, &mut lens, &mut codewords);

        // Should produce a valid 2-symbol code for symbols 0 and 1
        assert_eq!(lens[0], 1);
        assert_eq!(lens[1], 1);
    }

    #[test]
    fn test_make_huffman_code_length_limited() {
        // Create frequencies that would naturally produce codes longer than 7 bits
        let mut freqs = [0u32; 19];
        freqs[0] = 10000;
        freqs[1] = 5000;
        freqs[2] = 2500;
        freqs[3] = 1250;
        freqs[4] = 625;
        freqs[5] = 312;
        freqs[6] = 156;
        freqs[7] = 78;
        freqs[8] = 39;
        freqs[9] = 20;
        freqs[10] = 10;
        freqs[11] = 5;
        freqs[12] = 2;
        freqs[13] = 1;

        let mut lens = [0u8; 19];
        let mut codewords = [0u32; 19];
        make_huffman_code(19, 7, &freqs, &mut lens, &mut codewords);

        for &l in &lens {
            assert!(l <= 7, "code length {l} exceeds max of 7");
        }
    }
}

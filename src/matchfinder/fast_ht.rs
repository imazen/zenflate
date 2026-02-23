//! FastHt matchfinder: 2-entry hash table with limited hash updates.
//!
//! Combines the 2-entry bucket design from [`HtMatchfinder`](super::ht::HtMatchfinder)
//! with limited hash updates during match skips for better throughput.
//!
//! - 14-bit hash (16K buckets × 2 entries × 2 bytes = 64KB)
//! - Only updates 3 hash positions per match skip (vs all positions)
//! - Better match quality than [`TurboMatchfinder`](super::turbo::TurboMatchfinder)
//!   due to second entry per bucket
//!
//! Used by the `FastHt` strategy (effort 5-7).

use super::{MATCHFINDER_WINDOW_SIZE, lz_extend, lz_hash, matchfinder_init, matchfinder_rebase};

/// Hash order for the fast_ht matchfinder.
const FAST_HT_HASH_ORDER: u32 = 14;

/// Number of entries per hash bucket.
const BUCKET_SIZE: usize = 2;

/// Minimum match length.
pub(crate) const FAST_HT_MIN_MATCH_LEN: u32 = 4;

/// Minimum value of max_len for longest_match().
pub(crate) const FAST_HT_REQUIRED_NBYTES: u32 = 5;

/// Number of buckets in the hash table.
const NUM_BUCKETS: usize = 1 << FAST_HT_HASH_ORDER;

/// Maximum number of hash positions to update during match skips.
const SKIP_UPDATE_LIMIT: u32 = 3;

/// Hash order constant, exported for callers that precompute hashes.
pub(crate) const FAST_HT_MATCHFINDER_HASH_ORDER: u32 = FAST_HT_HASH_ORDER;

/// 2-entry hash table matchfinder with limited skip updates.
///
/// Each bucket stores two positions (i16). Provides better match quality
/// than the turbo matchfinder while still being faster than hash chains
/// due to limited hash updates on skips.
#[derive(Clone)]
pub(crate) struct FastHtMatchfinder {
    hash_tab: [[i16; BUCKET_SIZE]; NUM_BUCKETS],
}

impl FastHtMatchfinder {
    /// Create and initialize a new FastHtMatchfinder.
    pub fn new() -> Self {
        Self {
            hash_tab: [[super::MATCHFINDER_INITVAL; BUCKET_SIZE]; NUM_BUCKETS],
        }
    }

    /// Initialize (reset) the matchfinder.
    pub fn init(&mut self) {
        matchfinder_init(self.hash_tab.as_flattened_mut());
    }

    /// Slide the window by MATCHFINDER_WINDOW_SIZE.
    fn slide_window(&mut self) {
        matchfinder_rebase(self.hash_tab.as_flattened_mut());
    }

    /// Find the longest match at position `in_next`.
    ///
    /// Returns `(best_len, offset)` where `best_len` is 0 if no match found.
    #[inline(always)]
    pub fn longest_match(
        &mut self,
        input: &[u8],
        in_base_offset: &mut usize,
        in_next: usize,
        max_len: u32,
        nice_len: u32,
        next_hash: &mut u32,
    ) -> (u32, u32) {
        use crate::fast_bytes::{load_u32_le, prefetch};

        debug_assert!(max_len >= FAST_HT_REQUIRED_NBYTES);

        let mut cur_pos = (in_next - *in_base_offset) as i32;

        // Slide window if we've reached the boundary
        if cur_pos as u32 >= MATCHFINDER_WINDOW_SIZE {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos -= MATCHFINDER_WINDOW_SIZE as i32;
        }

        let in_base = *in_base_offset;
        let cutoff = cur_pos - MATCHFINDER_WINDOW_SIZE as i32;

        let hash = *next_hash as usize;

        // Precompute next hash and prefetch
        if in_next + 5 <= input.len() {
            *next_hash = lz_hash(load_u32_le(input, in_next + 1), FAST_HT_HASH_ORDER);
            prefetch(&self.hash_tab[*next_hash as usize]);
        }

        // Load 4 bytes at current position for quick comparison
        let seq = load_u32_le(input, in_next);

        // 2-entry hand-unrolled search (same as HtMatchfinder)
        let cur_node = self.hash_tab[hash][0] as i32;
        self.hash_tab[hash][0] = cur_pos as i16;

        if cur_node <= cutoff {
            return (0, 0);
        }

        let match_pos = (in_base as isize + cur_node as isize) as usize;
        let matchptr_seq = load_u32_le(input, match_pos);

        let to_insert = cur_node as i16;
        let cur_node2 = self.hash_tab[hash][1] as i32;
        self.hash_tab[hash][1] = to_insert;

        let mut best_len = 0u32;
        let mut best_offset = 0u32;

        if matchptr_seq == seq {
            best_len = lz_extend(&input[in_next..], &input[match_pos..], 4, max_len);
            best_offset = (in_next - match_pos) as u32;

            if cur_node2 <= cutoff || best_len >= nice_len {
                return (best_len, best_offset);
            }

            let match_pos2 = (in_base as isize + cur_node2 as isize) as usize;
            let matchptr2_seq = load_u32_le(input, match_pos2);

            // Check second entry: must match first 4 bytes AND bytes at best_len-3
            if matchptr2_seq == seq && best_len >= 4 {
                let tail_off = (best_len - 3) as usize;
                let s_tail = load_u32_le(input, in_next + tail_off);
                let m_tail = load_u32_le(input, match_pos2 + tail_off);
                if s_tail == m_tail {
                    let len2 = lz_extend(&input[in_next..], &input[match_pos2..], 4, max_len);
                    if len2 > best_len {
                        best_len = len2;
                        best_offset = (in_next - match_pos2) as u32;
                    }
                }
            }
        } else {
            if cur_node2 <= cutoff {
                return (0, 0);
            }
            let match_pos2 = (in_base as isize + cur_node2 as isize) as usize;
            let matchptr2_seq = load_u32_le(input, match_pos2);
            if matchptr2_seq == seq {
                best_len = lz_extend(&input[in_next..], &input[match_pos2..], 4, max_len);
                best_offset = (in_next - match_pos2) as u32;
            }
        }

        (best_len, best_offset)
    }

    /// Skip `count` bytes with limited hash updates.
    ///
    /// Only updates up to `SKIP_UPDATE_LIMIT` hash entries spread across
    /// the skipped region, instead of updating every position.
    #[inline(always)]
    pub fn skip_bytes(
        &mut self,
        input: &[u8],
        in_base_offset: &mut usize,
        in_next: usize,
        count: u32,
        next_hash: &mut u32,
    ) {
        use crate::fast_bytes::{load_u32_le, prefetch};

        if count == 0 {
            return;
        }

        let in_end = input.len();
        if (count + FAST_HT_REQUIRED_NBYTES) as usize > in_end - in_next {
            return;
        }

        let mut cur_pos = (in_next - *in_base_offset) as i32;

        if (cur_pos + count as i32 - 1) >= MATCHFINDER_WINDOW_SIZE as i32 {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos -= MATCHFINDER_WINDOW_SIZE as i32;
        }

        if count <= SKIP_UPDATE_LIMIT {
            // Short skip: update all positions (shift bucket like HtMatchfinder)
            let mut hash = *next_hash as usize;
            let mut pos = in_next;
            let mut remaining = count;
            while remaining > 0 {
                self.hash_tab[hash][1] = self.hash_tab[hash][0];
                self.hash_tab[hash][0] = cur_pos as i16;
                pos += 1;
                cur_pos += 1;
                remaining -= 1;
                if pos + 4 <= in_end {
                    hash = lz_hash(load_u32_le(input, pos), FAST_HT_HASH_ORDER) as usize;
                }
            }
            prefetch(&self.hash_tab[hash]);
            *next_hash = hash as u32;
        } else {
            // Long skip: update at start, middle, and end only
            // Start position
            let start_hash = *next_hash as usize;
            self.hash_tab[start_hash][1] = self.hash_tab[start_hash][0];
            self.hash_tab[start_hash][0] = cur_pos as i16;

            // Middle position
            let mid = count / 2;
            let mid_pos = in_next + mid as usize;
            if mid_pos + 4 <= in_end {
                let mid_hash =
                    lz_hash(load_u32_le(input, mid_pos), FAST_HT_HASH_ORDER) as usize;
                self.hash_tab[mid_hash][1] = self.hash_tab[mid_hash][0];
                self.hash_tab[mid_hash][0] = (cur_pos + mid as i32) as i16;
            }

            // End position (last skipped byte)
            let end_pos = in_next + count as usize - 1;
            if end_pos + 4 <= in_end {
                let end_hash =
                    lz_hash(load_u32_le(input, end_pos), FAST_HT_HASH_ORDER) as usize;
                self.hash_tab[end_hash][1] = self.hash_tab[end_hash][0];
                self.hash_tab[end_hash][0] = (cur_pos + count as i32 - 1) as i16;
            }

            // Compute next_hash for the position after the skip
            let after_pos = in_next + count as usize;
            if after_pos + 4 <= in_end {
                *next_hash = lz_hash(load_u32_le(input, after_pos), FAST_HT_HASH_ORDER);
                prefetch(&self.hash_tab[*next_hash as usize]);
            }
        }
    }
}

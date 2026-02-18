//! Hash Table (ht) matchfinder for the fastest compression level.
//!
//! Ported from libdeflate's `ht_matchfinder.h`.
//!
//! Stores hash chains inline in the hash table (2 entries per bucket).
//! Optimized for speed over compression ratio. Does not support length-3 matches.

use super::{
    MATCHFINDER_WINDOW_SIZE, lz_extend, lz_hash, matchfinder_init, matchfinder_rebase,
};

/// Hash table order (log2 of number of buckets).
const HT_MATCHFINDER_HASH_ORDER: u32 = 15;

/// Number of entries per hash bucket.
const HT_MATCHFINDER_BUCKET_SIZE: usize = 2;

/// Minimum match length for the ht_matchfinder.
pub(crate) const HT_MATCHFINDER_MIN_MATCH_LEN: u32 = 4;

/// Minimum value of max_len for longest_match().
pub(crate) const HT_MATCHFINDER_REQUIRED_NBYTES: u32 = 5;

/// Number of buckets in the hash table.
const HT_NUM_BUCKETS: usize = 1 << HT_MATCHFINDER_HASH_ORDER;

/// Hash Table matchfinder: 32K buckets × 2 entries (i16 positions).
pub(crate) struct HtMatchfinder {
    hash_tab: [[i16; HT_MATCHFINDER_BUCKET_SIZE]; HT_NUM_BUCKETS],
}

impl HtMatchfinder {
    /// Create and initialize a new HtMatchfinder.
    pub fn new() -> Self {
        Self {
            hash_tab: [[super::MATCHFINDER_INITVAL; HT_MATCHFINDER_BUCKET_SIZE]; HT_NUM_BUCKETS],
        }
    }

    /// Initialize (reset) the matchfinder.
    pub fn init(&mut self) {
        // Flatten and init — the memory layout is contiguous i16 values
        for bucket in self.hash_tab.iter_mut() {
            matchfinder_init(bucket);
        }
    }

    /// Slide the window by MATCHFINDER_WINDOW_SIZE.
    fn slide_window(&mut self) {
        for bucket in self.hash_tab.iter_mut() {
            matchfinder_rebase(bucket);
        }
    }

    /// Find the longest match at position `in_next` within the input buffer.
    ///
    /// `in_base_offset` is the current base offset into the input (adjusted after window slides).
    /// `in_next` is the absolute position in the input we're matching at.
    /// `max_len` is the maximum match length allowed (must be >= HT_MATCHFINDER_REQUIRED_NBYTES).
    /// `nice_len` is the "nice" length — stop searching if we find a match this long.
    /// `next_hash` is the precomputed hash for the next position (updated on return).
    ///
    /// Returns `(best_len, offset)` where `best_len` is 0 if no match found,
    /// and `offset` is the match offset (distance back).
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
        debug_assert!(max_len >= HT_MATCHFINDER_REQUIRED_NBYTES);

        let mut cur_pos = (in_next - *in_base_offset) as i32;

        // Slide window if we've reached the boundary
        if cur_pos as u32 == MATCHFINDER_WINDOW_SIZE {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos = 0;
        }

        let in_base = *in_base_offset;
        let cutoff = cur_pos - MATCHFINDER_WINDOW_SIZE as i32;

        let hash = *next_hash as usize;

        // Precompute next hash from in_next+1
        if in_next + 5 <= input.len() {
            let seq_next = u32::from_le_bytes(
                input[in_next + 1..in_next + 5].try_into().unwrap(),
            );
            *next_hash = lz_hash(seq_next, HT_MATCHFINDER_HASH_ORDER);
        }

        // Load 4 bytes at current position for quick comparison
        let seq = u32::from_le_bytes(
            input[in_next..in_next + 4].try_into().unwrap(),
        );

        // BUCKET_SIZE == 2 hand-unrolled path (matches C code)
        let cur_node = self.hash_tab[hash][0] as i32;
        self.hash_tab[hash][0] = cur_pos as i16;

        if cur_node <= cutoff {
            return (0, 0);
        }

        let match_pos = in_base + cur_node as usize;
        let matchptr_seq = u32::from_le_bytes(
            input[match_pos..match_pos + 4].try_into().unwrap(),
        );

        let to_insert = cur_node as i16;
        let cur_node2 = self.hash_tab[hash][1] as i32;
        self.hash_tab[hash][1] = to_insert;

        let mut best_len = 0u32;
        let mut best_offset = 0u32;

        if matchptr_seq == seq {
            best_len = lz_extend(
                &input[in_next..],
                &input[match_pos..],
                4,
                max_len,
            );
            best_offset = (in_next - match_pos) as u32;

            if cur_node2 <= cutoff || best_len >= nice_len {
                return (best_len, best_offset);
            }

            let match_pos2 = in_base + cur_node2 as usize;
            let matchptr2_seq = u32::from_le_bytes(
                input[match_pos2..match_pos2 + 4].try_into().unwrap(),
            );

            // Check second entry: must match first 4 bytes AND the bytes at best_len-3
            if matchptr2_seq == seq && best_len >= 4 {
                let tail_off = (best_len - 3) as usize;
                let s_tail = u32::from_le_bytes(
                    input[in_next + tail_off..in_next + tail_off + 4]
                        .try_into()
                        .unwrap(),
                );
                let m_tail = u32::from_le_bytes(
                    input[match_pos2 + tail_off..match_pos2 + tail_off + 4]
                        .try_into()
                        .unwrap(),
                );
                if s_tail == m_tail {
                    let len2 = lz_extend(
                        &input[in_next..],
                        &input[match_pos2..],
                        4,
                        max_len,
                    );
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
            let match_pos2 = in_base + cur_node2 as usize;
            let matchptr2_seq = u32::from_le_bytes(
                input[match_pos2..match_pos2 + 4].try_into().unwrap(),
            );
            if matchptr2_seq == seq {
                best_len = lz_extend(
                    &input[in_next..],
                    &input[match_pos2..],
                    4,
                    max_len,
                );
                best_offset = (in_next - match_pos2) as u32;
            }
        }

        (best_len, best_offset)
    }

    /// Skip `count` bytes in the matchfinder (update hash table without finding matches).
    #[inline(always)]
    pub fn skip_bytes(
        &mut self,
        input: &[u8],
        in_base_offset: &mut usize,
        in_next: usize,
        count: u32,
        next_hash: &mut u32,
    ) {
        if count == 0 {
            return;
        }

        let in_end = input.len();
        if (count + HT_MATCHFINDER_REQUIRED_NBYTES) as usize > in_end - in_next {
            return;
        }

        let mut cur_pos = (in_next - *in_base_offset) as i32;

        if (cur_pos + count as i32 - 1) >= MATCHFINDER_WINDOW_SIZE as i32 {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos -= MATCHFINDER_WINDOW_SIZE as i32;
        }

        let mut hash = *next_hash as usize;
        let mut pos = in_next;
        let mut remaining = count;

        while remaining > 0 {
            // Shift bucket: move [0] to [1]
            self.hash_tab[hash][1] = self.hash_tab[hash][0];
            self.hash_tab[hash][0] = cur_pos as i16;

            pos += 1;
            cur_pos += 1;
            remaining -= 1;

            if pos + 4 <= in_end {
                hash = lz_hash(
                    u32::from_le_bytes(input[pos..pos + 4].try_into().unwrap()),
                    HT_MATCHFINDER_HASH_ORDER,
                ) as usize;
            }
        }

        *next_hash = hash as u32;
    }
}

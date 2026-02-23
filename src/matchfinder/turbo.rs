//! Turbo matchfinder: single-entry hash table with limited hash updates.
//!
//! Faster than [`HtMatchfinder`](super::ht::HtMatchfinder) by:
//! - Single entry per bucket (32K × 2 bytes = 64KB vs 128KB)
//! - Only updating 3 hash positions per match skip (vs all positions)
//! - Simpler lookup path (no second entry to check)
//!
//! Used by the `StaticTurbo` (effort 1-2) and `Turbo` (effort 3-4) strategies.

use super::{MATCHFINDER_WINDOW_SIZE, lz_extend, lz_hash, matchfinder_init, matchfinder_rebase};

/// Hash order for the turbo matchfinder.
const TURBO_HASH_ORDER: u32 = 15;

/// Minimum match length for the turbo matchfinder.
pub(crate) const TURBO_MIN_MATCH_LEN: u32 = 4;

/// Minimum value of max_len for longest_match().
pub(crate) const TURBO_REQUIRED_NBYTES: u32 = 5;

/// Number of buckets in the hash table.
const TURBO_NUM_BUCKETS: usize = 1 << TURBO_HASH_ORDER;

/// Maximum number of hash positions to update during match skips.
const SKIP_UPDATE_LIMIT: u32 = 3;

/// Hash order constant, exported for callers that precompute hashes.
pub(crate) const TURBO_MATCHFINDER_HASH_ORDER: u32 = TURBO_HASH_ORDER;

/// Single-entry hash table matchfinder optimized for speed.
///
/// Each bucket stores one position (i16). On collision, the old entry is lost.
/// Match skips only update a limited number of hash entries for speed.
#[derive(Clone)]
pub(crate) struct TurboMatchfinder {
    hash_tab: [i16; TURBO_NUM_BUCKETS],
}

impl TurboMatchfinder {
    /// Create and initialize a new TurboMatchfinder.
    pub fn new() -> Self {
        Self {
            hash_tab: [super::MATCHFINDER_INITVAL; TURBO_NUM_BUCKETS],
        }
    }

    /// Initialize (reset) the matchfinder.
    pub fn init(&mut self) {
        matchfinder_init(&mut self.hash_tab);
    }

    /// Slide the window by MATCHFINDER_WINDOW_SIZE.
    fn slide_window(&mut self) {
        matchfinder_rebase(&mut self.hash_tab);
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

        debug_assert!(max_len >= TURBO_REQUIRED_NBYTES);

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
            *next_hash = lz_hash(load_u32_le(input, in_next + 1), TURBO_HASH_ORDER);
            prefetch(&self.hash_tab[*next_hash as usize]);
        }

        // Load 4 bytes at current position for quick comparison
        let seq = load_u32_le(input, in_next);

        // Single entry: read and replace
        let prev_pos = self.hash_tab[hash] as i32;
        self.hash_tab[hash] = cur_pos as i16;

        if prev_pos <= cutoff {
            return (0, 0);
        }

        let match_pos = (in_base as isize + prev_pos as isize) as usize;
        let match_seq = load_u32_le(input, match_pos);

        if match_seq != seq {
            return (0, 0);
        }

        let best_len = lz_extend(&input[in_next..], &input[match_pos..], 4, max_len);
        let _ = nice_len; // Single entry — nothing more to search
        let best_offset = (in_next - match_pos) as u32;
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
        if (count + TURBO_REQUIRED_NBYTES) as usize > in_end - in_next {
            return;
        }

        let mut cur_pos = (in_next - *in_base_offset) as i32;

        if (cur_pos + count as i32 - 1) >= MATCHFINDER_WINDOW_SIZE as i32 {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos -= MATCHFINDER_WINDOW_SIZE as i32;
        }

        if count <= SKIP_UPDATE_LIMIT {
            // Short skip: update all positions
            let mut hash = *next_hash as usize;
            let mut pos = in_next;
            let mut remaining = count;
            while remaining > 0 {
                self.hash_tab[hash] = cur_pos as i16;
                pos += 1;
                cur_pos += 1;
                remaining -= 1;
                if pos + 4 <= in_end {
                    hash = lz_hash(load_u32_le(input, pos), TURBO_HASH_ORDER) as usize;
                }
            }
            prefetch(&self.hash_tab[hash]);
            *next_hash = hash as u32;
        } else {
            // Long skip: update at start, middle, and end only
            // Position 0 (start of skip)
            self.hash_tab[*next_hash as usize] = cur_pos as i16;

            // Position at middle
            let mid = count / 2;
            let mid_pos = in_next + mid as usize;
            if mid_pos + 4 <= in_end {
                let mid_hash = lz_hash(load_u32_le(input, mid_pos), TURBO_HASH_ORDER) as usize;
                self.hash_tab[mid_hash] = (cur_pos + mid as i32) as i16;
            }

            // Position at end (last skipped byte)
            let end_pos = in_next + count as usize - 1;
            if end_pos + 4 <= in_end {
                let end_hash = lz_hash(load_u32_le(input, end_pos), TURBO_HASH_ORDER) as usize;
                self.hash_tab[end_hash] = (cur_pos + count as i32 - 1) as i16;
            }

            // Compute next_hash for the position after the skip
            let after_pos = in_next + count as usize;
            if after_pos + 4 <= in_end {
                *next_hash = lz_hash(load_u32_le(input, after_pos), TURBO_HASH_ORDER);
                prefetch(&self.hash_tab[*next_hash as usize]);
            }
        }
    }
}

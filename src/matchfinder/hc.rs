//! Hash Chains (hc) matchfinder for greedy, lazy, and lazy2 compression.
//!
//! Ported from libdeflate's `hc_matchfinder.h`.
//!
//! Uses a separate hash table for length-3 matches (`hash3_tab`, 32K entries)
//! and a hash table of linked lists for length-4+ matches (`hash4_tab`, 64K entries).
//! Chain links are stored in `next_tab` (32K entries, indexed by position mod window).

use super::{MATCHFINDER_WINDOW_SIZE, lz_extend, lz_hash, matchfinder_init, matchfinder_rebase};

/// Hash order for length 3 matches.
const HC_MATCHFINDER_HASH3_ORDER: u32 = 15;

/// Hash order for length 4+ matches.
pub(crate) const HC_MATCHFINDER_HASH4_ORDER: u32 = 16;

/// Number of entries in hash3_tab.
const HC_HASH3_SIZE: usize = 1 << HC_MATCHFINDER_HASH3_ORDER;

/// Number of entries in hash4_tab.
const HC_HASH4_SIZE: usize = 1 << HC_MATCHFINDER_HASH4_ORDER;

/// Window mask for chain link indexing.
const WINDOW_MASK: usize = MATCHFINDER_WINDOW_SIZE as usize - 1;

/// Hash Chains matchfinder for levels 2-9.
pub(crate) struct HcMatchfinder {
    /// Hash table for length 3 matches (singleton entries).
    hash3_tab: [i16; HC_HASH3_SIZE],
    /// Hash table for length 4+ matches (chain heads).
    hash4_tab: [i16; HC_HASH4_SIZE],
    /// Chain links: next_tab[pos & WINDOW_MASK] = next position in chain.
    next_tab: [i16; MATCHFINDER_WINDOW_SIZE as usize],
}

impl HcMatchfinder {
    /// Create and initialize a new HcMatchfinder.
    pub fn new() -> Self {
        Self {
            hash3_tab: [super::MATCHFINDER_INITVAL; HC_HASH3_SIZE],
            hash4_tab: [super::MATCHFINDER_INITVAL; HC_HASH4_SIZE],
            next_tab: [super::MATCHFINDER_INITVAL; MATCHFINDER_WINDOW_SIZE as usize],
        }
    }

    /// Initialize (reset) the matchfinder for a new input buffer.
    pub fn init(&mut self) {
        // Only hash tables need initialization; next_tab entries are written before read.
        // But rebasing touches next_tab too, so we init everything for safety.
        matchfinder_init(&mut self.hash3_tab);
        matchfinder_init(&mut self.hash4_tab);
    }

    /// Slide the window by MATCHFINDER_WINDOW_SIZE.
    fn slide_window(&mut self) {
        matchfinder_rebase(&mut self.hash3_tab);
        matchfinder_rebase(&mut self.hash4_tab);
        matchfinder_rebase(&mut self.next_tab);
    }

    /// Find the longest match longer than `best_len` bytes.
    ///
    /// `best_len` is the minimum length to beat (0 for greedy, or a previously
    /// found match length for lazy evaluation). Returns `(new_best_len, offset)`
    /// where `new_best_len` may equal `best_len` if no better match was found,
    /// and `offset` is the match distance (only valid if `new_best_len > 0`).
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub fn longest_match(
        &mut self,
        input: &[u8],
        in_base_offset: &mut usize,
        in_next: usize,
        best_len: u32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
    ) -> (u32, u32) {
        use crate::fast_bytes::load_u32_le;

        let mut best_len = best_len;
        let mut best_offset = 0u32;
        let mut depth_remaining = max_search_depth;

        let mut cur_pos = (in_next - *in_base_offset) as u32;

        if cur_pos == MATCHFINDER_WINDOW_SIZE {
            self.slide_window();
            *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
            cur_pos = 0;
        }

        let in_base = *in_base_offset;
        let cutoff = cur_pos as i32 - MATCHFINDER_WINDOW_SIZE as i32;

        let hash3 = next_hashes[0] as usize;
        let hash4 = next_hashes[1] as usize;

        let cur_node3 = self.hash3_tab[hash3] as i32;
        let mut cur_node4 = self.hash4_tab[hash4] as i32;

        // Update hash3: replace singleton
        self.hash3_tab[hash3] = cur_pos as i16;
        // Update hash4: prepend to chain
        self.hash4_tab[hash4] = cur_pos as i16;
        self.next_tab[cur_pos as usize] = cur_node4 as i16;

        // Precompute next hashes
        if in_next + 5 <= input.len() {
            let next_seq = load_u32_le(input, in_next + 1);
            next_hashes[0] = lz_hash(next_seq & 0xFFFFFF, HC_MATCHFINDER_HASH3_ORDER);
            next_hashes[1] = lz_hash(next_seq, HC_MATCHFINDER_HASH4_ORDER);
        }

        // Need at least 5 bytes for match searching (hash tables already updated above)
        if max_len < 5 {
            return (best_len, best_offset);
        }

        if best_len < 4 {
            // No length 4+ match yet. Check length 3, then search chain for length 4.

            // Heuristic: if hash3 has nothing in-window, skip everything.
            // hash3 is broader (fewer bits), so if it's empty, hash4 is unlikely
            // to have useful matches either.
            if cur_node3 <= cutoff {
                return (best_len, best_offset);
            }

            let seq4 = load_u32_le(input, in_next);

            // Check for a length 3 match (hash3 is a singleton, not a chain)
            if best_len < 3 {
                let match_pos = (in_base as isize + cur_node3 as isize) as usize;
                let match_seq = load_u32_le(input, match_pos);
                if (match_seq & 0xFFFFFF) == (seq4 & 0xFFFFFF) {
                    best_len = 3;
                    best_offset = (in_next - match_pos) as u32;
                }
            }

            // Search hash4 chain for first length 4+ match
            if cur_node4 <= cutoff {
                return (best_len, best_offset);
            }

            loop {
                let match_pos = (in_base as isize + cur_node4 as isize) as usize;
                let match_seq = load_u32_le(input, match_pos);

                if match_seq == seq4 {
                    // Found length 4+ match — extend it
                    best_len = lz_extend(&input[in_next..], &input[match_pos..], 4, max_len);
                    best_offset = (in_next - match_pos) as u32;
                    if best_len >= nice_len {
                        return (best_len, best_offset);
                    }
                    cur_node4 = self.next_tab[cur_node4 as usize & WINDOW_MASK] as i32;
                    if cur_node4 <= cutoff || {
                        depth_remaining -= 1;
                        depth_remaining == 0
                    } {
                        return (best_len, best_offset);
                    }
                    break; // Fall through to length 5+ search
                }

                cur_node4 = self.next_tab[cur_node4 as usize & WINDOW_MASK] as i32;
                if cur_node4 <= cutoff || {
                    depth_remaining -= 1;
                    depth_remaining == 0
                } {
                    return (best_len, best_offset);
                }
            }
        } else {
            // Already have length 4+ from a previous call (lazy evaluation)
            if cur_node4 <= cutoff || best_len >= nice_len {
                return (best_len, best_offset);
            }
        }

        // Search chain for matches longer than best_len
        loop {
            let match_pos = (in_base as isize + cur_node4 as isize) as usize;

            // Quick rejection: check last 4 bytes and first 4 bytes
            let tail_off = (best_len - 3) as usize;
            let m_tail = load_u32_le(input, match_pos + tail_off);
            let s_tail = load_u32_le(input, in_next + tail_off);

            if m_tail == s_tail {
                let m_head = load_u32_le(input, match_pos);
                let s_head = load_u32_le(input, in_next);
                if m_head == s_head {
                    // Full extension
                    let len = lz_extend(&input[in_next..], &input[match_pos..], 4, max_len);
                    if len > best_len {
                        best_len = len;
                        best_offset = (in_next - match_pos) as u32;
                        if best_len >= nice_len {
                            return (best_len, best_offset);
                        }
                    }
                }
            }

            cur_node4 = self.next_tab[cur_node4 as usize & WINDOW_MASK] as i32;
            if cur_node4 <= cutoff || {
                depth_remaining -= 1;
                depth_remaining == 0
            } {
                return (best_len, best_offset);
            }
        }
    }

    /// Skip `count` bytes, updating hash tables without searching for matches.
    #[inline(always)]
    pub fn skip_bytes(
        &mut self,
        input: &[u8],
        in_base_offset: &mut usize,
        in_next: usize,
        in_end: usize,
        count: u32,
        next_hashes: &mut [u32; 2],
    ) {
        use crate::fast_bytes::load_u32_le;

        if count as usize + 5 > in_end - in_next {
            return;
        }

        let mut cur_pos = (in_next - *in_base_offset) as u32;
        let mut hash3 = next_hashes[0] as usize;
        let mut hash4 = next_hashes[1] as usize;
        let mut pos = in_next;
        let mut remaining = count;

        loop {
            if cur_pos == MATCHFINDER_WINDOW_SIZE {
                self.slide_window();
                *in_base_offset += MATCHFINDER_WINDOW_SIZE as usize;
                cur_pos = 0;
            }

            // Insert current position: update hash3, prepend to hash4 chain
            self.hash3_tab[hash3] = cur_pos as i16;
            self.next_tab[cur_pos as usize] = self.hash4_tab[hash4];
            self.hash4_tab[hash4] = cur_pos as i16;

            pos += 1;
            cur_pos += 1;
            remaining -= 1;

            let next_seq = load_u32_le(input, pos);
            hash3 = lz_hash(next_seq & 0xFFFFFF, HC_MATCHFINDER_HASH3_ORDER) as usize;
            hash4 = lz_hash(next_seq, HC_MATCHFINDER_HASH4_ORDER) as usize;

            if remaining == 0 {
                break;
            }
        }

        next_hashes[0] = hash3 as u32;
        next_hashes[1] = hash4 as u32;
    }
}

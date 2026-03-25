//! Binary Trees (bt) matchfinder for near-optimal compression.
//!
//! Ported from libdeflate's `bt_matchfinder.h`.
//!
//! Uses a hash table of binary trees where each bucket contains sequences
//! whose first 4 bytes share the same hash code. Each tree is sorted
//! lexicographically: left children are lesser, right children are greater.
//!
//! At each byte position, a new node is created, the tree is traversed to
//! find matches and re-rooted at the new node. Compared to hash chains,
//! the binary tree finds more matches per step (ideally log(n) vs n)
//! but requires nearly twice as much memory.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

#[cfg(not(feature = "unchecked"))]
use super::lz_extend;
use super::{
    MATCHFINDER_INITVAL, MATCHFINDER_WINDOW_SIZE, lz_hash, matchfinder_init, matchfinder_rebase,
};

/// Hash order for length 3 matches.
const BT_MATCHFINDER_HASH3_ORDER: u32 = 16;

/// Number of ways per hash3 bucket.
const BT_MATCHFINDER_HASH3_WAYS: usize = 2;

/// Hash order for length 4+ matches (binary tree roots).
const BT_MATCHFINDER_HASH4_ORDER: u32 = 16;

/// Number of hash3 buckets.
const BT_HASH3_SIZE: usize = 1 << BT_MATCHFINDER_HASH3_ORDER;

/// Number of hash4 buckets.
const BT_HASH4_SIZE: usize = 1 << BT_MATCHFINDER_HASH4_ORDER;

/// Minimum bytes remaining required for get_matches() / skip_byte().
pub(crate) const BT_MATCHFINDER_REQUIRED_NBYTES: u32 = 5;

/// Window mask for child table indexing.
const WINDOW_MASK: usize = MATCHFINDER_WINDOW_SIZE as usize - 1;

// Always use Vec. The `unchecked` feature controls access patterns, not storage.
type BtHash3Tab = Vec<i16>;
type BtHash4Tab = Vec<i16>;
type BtChildTab = Vec<i16>;

/// A match found by the bt_matchfinder.
#[derive(Clone, Copy, Default)]
pub(crate) struct LzMatch {
    /// Number of bytes matched.
    pub length: u16,
    /// Offset back from the current position.
    pub offset: u16,
}

/// Binary tree matchfinder for near-optimal compression (levels 10-12).
///
/// With `unchecked`, uses fixed arrays so the whole NearOptimalState
/// becomes a single allocation via `Box::new_uninit()`.
/// Without `unchecked`, uses Vec (separate heap allocations per field).
///
#[derive(Clone)]
pub(crate) struct BtMatchfinder {
    /// Hash table for length 3 matches (2-way). Flat: [BT_HASH3_SIZE * 2].
    hash3_tab: BtHash3Tab,
    /// Hash table containing roots of binary trees for length 4+ matches.
    hash4_tab: BtHash4Tab,
    /// Child node references: left child at [2*(pos & WINDOW_MASK)],
    /// right child at [2*(pos & WINDOW_MASK) + 1].
    child_tab: BtChildTab,
}

impl BtMatchfinder {
    pub fn new() -> Self {
        Self {
            hash3_tab: alloc::vec![MATCHFINDER_INITVAL; BT_HASH3_SIZE * BT_MATCHFINDER_HASH3_WAYS],
            hash4_tab: alloc::vec![MATCHFINDER_INITVAL; BT_HASH4_SIZE],
            child_tab: alloc::vec![MATCHFINDER_INITVAL; 2 * MATCHFINDER_WINDOW_SIZE as usize],
        }
    }

    /// Initialize (reset) the matchfinder for a new input buffer.
    /// Only hash tables are reset; child_tab entries are written before read.
    pub fn init(&mut self) {
        matchfinder_init(&mut self.hash3_tab);
        matchfinder_init(&mut self.hash4_tab);
    }

    /// Slide the window by MATCHFINDER_WINDOW_SIZE.
    /// Rebases all tables including child_tab.
    pub fn slide_window(&mut self) {
        matchfinder_rebase(&mut self.hash3_tab);
        matchfinder_rebase(&mut self.hash4_tab);
        matchfinder_rebase(&mut self.child_tab);
    }

    #[inline(always)]
    fn left_child_idx(node: i32) -> usize {
        2 * (node as usize & WINDOW_MASK)
    }

    #[inline(always)]
    fn right_child_idx(node: i32) -> usize {
        2 * (node as usize & WINDOW_MASK) + 1
    }

    /// Get matches at the current position.
    ///
    /// `input` is the full input buffer.
    /// `in_base_offset` is the absolute offset of the window base in input.
    /// `cur_pos` is the position relative to the window base.
    /// `max_len` must be >= BT_MATCHFINDER_REQUIRED_NBYTES.
    /// `nice_len` is the "nice" length: stop if we find a match this long.
    ///
    /// Returns the number of matches written to `matches`.
    /// Matches are sorted by strictly increasing length.
    #[cfg_attr(feature = "unchecked", allow(dead_code))]
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub fn get_matches(
        &mut self,
        input: &[u8],
        in_base_offset: usize,
        cur_pos: i32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
        matches: &mut [LzMatch],
    ) -> usize {
        self.advance_one_byte::<true>(
            input,
            in_base_offset,
            cur_pos,
            max_len,
            nice_len,
            max_search_depth,
            next_hashes,
            matches,
        )
    }

    /// Skip one byte: maintain the tree structure without recording matches.
    #[cfg_attr(feature = "unchecked", allow(dead_code))]
    #[inline(always)]
    pub fn skip_byte(
        &mut self,
        input: &[u8],
        in_base_offset: usize,
        cur_pos: i32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
    ) {
        self.advance_one_byte::<false>(
            input,
            in_base_offset,
            cur_pos,
            nice_len, // max_len = nice_len for skip (matches C behavior)
            nice_len,
            max_search_depth,
            next_hashes,
            &mut [],
        );
    }

    /// Core method: advance one byte, optionally recording matches.
    ///
    /// When RECORD_MATCHES=true, finds all matches and writes them to `matches`.
    /// When RECORD_MATCHES=false, only maintains tree structure (skip).
    #[cfg_attr(feature = "unchecked", allow(dead_code))]
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn advance_one_byte<const RECORD_MATCHES: bool>(
        &mut self,
        input: &[u8],
        in_base_offset: usize,
        cur_pos: i32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
        matches: &mut [LzMatch],
    ) -> usize {
        #[cfg(not(feature = "unchecked"))]
        use crate::fast_bytes::get_byte;
        use crate::fast_bytes::{load_u32_le, prefetch};

        let in_next = (in_base_offset as isize + cur_pos as isize) as usize;
        let mut depth_remaining = max_search_depth;
        let cutoff = cur_pos - MATCHFINDER_WINDOW_SIZE as i32;
        let mut match_count = 0usize;
        let mut best_len = 3u32;

        // Precompute next position's hashes
        let next_hashseq = load_u32_le(input, in_next + 1);
        let hash3 = next_hashes[0] as usize;
        let hash4 = next_hashes[1] as usize;
        next_hashes[0] = lz_hash(next_hashseq & 0xFFFFFF, BT_MATCHFINDER_HASH3_ORDER);
        next_hashes[1] = lz_hash(next_hashseq, BT_MATCHFINDER_HASH4_ORDER);
        prefetch(&self.hash3_tab[next_hashes[0] as usize * BT_MATCHFINDER_HASH3_WAYS]);
        prefetch(&self.hash4_tab[next_hashes[1] as usize]);

        // Hash3: 2-way check for length 3 matches
        let h3 = hash3 * BT_MATCHFINDER_HASH3_WAYS;
        let cur_node_0 = self.hash3_tab[h3] as i32;
        self.hash3_tab[h3] = cur_pos as i16;
        let cur_node_1 = self.hash3_tab[h3 + 1] as i32;
        self.hash3_tab[h3 + 1] = cur_node_0 as i16;

        if RECORD_MATCHES && cur_node_0 > cutoff {
            // Load 4 bytes and mask to 3 — matches C pattern and requires only one load
            let seq3 = load_u32_le(input, in_next) & 0xFFFFFF;
            let match0_pos = (in_base_offset as isize + cur_node_0 as isize) as usize;
            if seq3 == load_u32_le(input, match0_pos) & 0xFFFFFF {
                matches[match_count] = LzMatch {
                    length: 3,
                    offset: (in_next - match0_pos) as u16,
                };
                match_count += 1;
            } else if cur_node_1 > cutoff {
                let match1_pos = (in_base_offset as isize + cur_node_1 as isize) as usize;
                if seq3 == load_u32_le(input, match1_pos) & 0xFFFFFF {
                    matches[match_count] = LzMatch {
                        length: 3,
                        offset: (in_next - match1_pos) as u16,
                    };
                    match_count += 1;
                }
            }
        }

        // Hash4: binary tree for length 4+ matches
        let mut cur_node = self.hash4_tab[hash4] as i32;
        self.hash4_tab[hash4] = cur_pos as i16;

        let mut pending_lt_idx = Self::left_child_idx(cur_pos);
        let mut pending_gt_idx = Self::right_child_idx(cur_pos);

        if cur_node <= cutoff {
            self.child_tab[pending_lt_idx] = MATCHFINDER_INITVAL;
            self.child_tab[pending_gt_idx] = MATCHFINDER_INITVAL;
            return match_count;
        }

        let mut best_lt_len = 0u32;
        let mut best_gt_len = 0u32;
        let mut len = 0u32;

        // Raw-pointer inner loop: eliminates fat-pointer register pressure.
        // input (&[u8] = 2 regs) → input_ptr (*const u8 = 1 reg), freeing
        // a register in this spill-heavy loop (~16 live values, 14 GPRs).
        #[cfg(feature = "unchecked")]
        {
            use super::raw::lz_extend_raw;

            let input_ptr = input.as_ptr();
            let child_ptr = self.child_tab.as_mut_ptr();

            // SAFETY: All offsets are bounded by in_end <= input.len() (checked
            // by callers) and child_tab indices are masked to WINDOW_MASK.
            unsafe {
                loop {
                    let match_pos = (in_base_offset as isize + cur_node as isize) as usize;

                    if *input_ptr.add(match_pos + len as usize)
                        == *input_ptr.add(in_next + len as usize)
                    {
                        len = lz_extend_raw(
                            input_ptr.add(in_next),
                            input_ptr.add(match_pos),
                            len + 1,
                            max_len,
                        );
                        if !RECORD_MATCHES || len > best_len {
                            if RECORD_MATCHES {
                                best_len = len;
                                matches[match_count] = LzMatch {
                                    length: len as u16,
                                    offset: (in_next - match_pos) as u16,
                                };
                                match_count += 1;
                            }
                            if len >= nice_len {
                                *child_ptr.add(pending_lt_idx) =
                                    *child_ptr.add(Self::left_child_idx(cur_node));
                                *child_ptr.add(pending_gt_idx) =
                                    *child_ptr.add(Self::right_child_idx(cur_node));
                                return match_count;
                            }
                        }
                    }

                    if *input_ptr.add(match_pos + len as usize)
                        < *input_ptr.add(in_next + len as usize)
                    {
                        *child_ptr.add(pending_lt_idx) = cur_node as i16;
                        pending_lt_idx = Self::right_child_idx(cur_node);
                        cur_node = *child_ptr.add(pending_lt_idx) as i32;
                        best_lt_len = len;
                        if best_gt_len < len {
                            len = best_gt_len;
                        }
                    } else {
                        *child_ptr.add(pending_gt_idx) = cur_node as i16;
                        pending_gt_idx = Self::left_child_idx(cur_node);
                        cur_node = *child_ptr.add(pending_gt_idx) as i32;
                        best_gt_len = len;
                        if best_lt_len < len {
                            len = best_lt_len;
                        }
                    }

                    depth_remaining -= 1;
                    if cur_node <= cutoff || depth_remaining == 0 {
                        *child_ptr.add(pending_lt_idx) = MATCHFINDER_INITVAL;
                        *child_ptr.add(pending_gt_idx) = MATCHFINDER_INITVAL;
                        return match_count;
                    }
                }
            }
        }

        #[cfg(not(feature = "unchecked"))]
        loop {
            let match_pos = (in_base_offset as isize + cur_node as isize) as usize;

            if get_byte(input, match_pos + len as usize) == get_byte(input, in_next + len as usize)
            {
                len = lz_extend(&input[in_next..], &input[match_pos..], len + 1, max_len);
                if !RECORD_MATCHES || len > best_len {
                    if RECORD_MATCHES {
                        best_len = len;
                        matches[match_count] = LzMatch {
                            length: len as u16,
                            offset: (in_next - match_pos) as u16,
                        };
                        match_count += 1;
                    }
                    if len >= nice_len {
                        self.child_tab[pending_lt_idx] =
                            self.child_tab[Self::left_child_idx(cur_node)];
                        self.child_tab[pending_gt_idx] =
                            self.child_tab[Self::right_child_idx(cur_node)];
                        return match_count;
                    }
                }
            }

            if get_byte(input, match_pos + len as usize) < get_byte(input, in_next + len as usize) {
                self.child_tab[pending_lt_idx] = cur_node as i16;
                pending_lt_idx = Self::right_child_idx(cur_node);
                cur_node = self.child_tab[pending_lt_idx] as i32;
                best_lt_len = len;
                if best_gt_len < len {
                    len = best_gt_len;
                }
            } else {
                self.child_tab[pending_gt_idx] = cur_node as i16;
                pending_gt_idx = Self::left_child_idx(cur_node);
                cur_node = self.child_tab[pending_gt_idx] as i32;
                best_gt_len = len;
                if best_lt_len < len {
                    len = best_lt_len;
                }
            }

            depth_remaining -= 1;
            if cur_node <= cutoff || depth_remaining == 0 {
                self.child_tab[pending_lt_idx] = MATCHFINDER_INITVAL;
                self.child_tab[pending_gt_idx] = MATCHFINDER_INITVAL;
                return match_count;
            }
        }
    }

    /// Raw-pointer get_matches: eliminates fat-pointer register pressure.
    ///
    /// Takes `*const u8` instead of `&[u8]` and `*mut LzMatch` instead of
    /// `&mut [LzMatch]`, freeing 2 registers in the caller's inlined hot loop.
    ///
    /// # Safety
    ///
    /// `input_ptr` must be valid for reads of at least `in_base_offset + cur_pos + max_len` bytes.
    /// `matches_ptr` must be valid for writes of at least `MAX_MATCHES_PER_POS` entries.
    #[cfg(feature = "unchecked")]
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn get_matches_raw(
        &mut self,
        input_ptr: *const u8,
        in_base_offset: usize,
        cur_pos: i32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
        matches_ptr: *mut LzMatch,
    ) -> usize {
        unsafe {
            self.advance_one_byte_raw::<true>(
                input_ptr,
                in_base_offset,
                cur_pos,
                max_len,
                nice_len,
                max_search_depth,
                next_hashes,
                matches_ptr,
            )
        }
    }

    /// Raw-pointer skip_byte: maintains tree structure without recording matches.
    ///
    /// # Safety
    ///
    /// `input_ptr` must be valid for reads of at least `in_base_offset + cur_pos + nice_len` bytes.
    #[cfg(feature = "unchecked")]
    #[inline(always)]
    pub unsafe fn skip_byte_raw(
        &mut self,
        input_ptr: *const u8,
        in_base_offset: usize,
        cur_pos: i32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
    ) {
        unsafe {
            self.advance_one_byte_raw::<false>(
                input_ptr,
                in_base_offset,
                cur_pos,
                nice_len,
                nice_len,
                max_search_depth,
                next_hashes,
                core::ptr::null_mut(),
            );
        }
    }

    /// Core raw-pointer method: advance one byte with all raw pointer access.
    ///
    /// Eliminates all fat pointers and bounds checks. Hash table access uses
    /// `get_unchecked`, input access uses raw pointer arithmetic.
    #[cfg(feature = "unchecked")]
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    unsafe fn advance_one_byte_raw<const RECORD_MATCHES: bool>(
        &mut self,
        input_ptr: *const u8,
        in_base_offset: usize,
        cur_pos: i32,
        max_len: u32,
        nice_len: u32,
        max_search_depth: u32,
        next_hashes: &mut [u32; 2],
        matches_ptr: *mut LzMatch,
    ) -> usize {
        use super::raw::lz_extend_raw;
        use crate::fast_bytes::{load_u32_le_ptr, prefetch_ptr};

        unsafe {
            let in_next = (in_base_offset as isize + cur_pos as isize) as usize;
            let mut depth_remaining = max_search_depth;
            let cutoff = cur_pos - MATCHFINDER_WINDOW_SIZE as i32;
            let mut match_count = 0usize;
            let mut best_len = 3u32;

            // Precompute next position's hashes
            let next_hashseq = load_u32_le_ptr(input_ptr, in_next + 1);
            let hash3 = next_hashes[0] as usize;
            let hash4 = next_hashes[1] as usize;
            next_hashes[0] = lz_hash(next_hashseq & 0xFFFFFF, BT_MATCHFINDER_HASH3_ORDER);
            next_hashes[1] = lz_hash(next_hashseq, BT_MATCHFINDER_HASH4_ORDER);
            prefetch_ptr(
                self.hash3_tab
                    .as_ptr()
                    .add(next_hashes[0] as usize * BT_MATCHFINDER_HASH3_WAYS)
                    as *const u8,
            );
            prefetch_ptr(self.hash4_tab.as_ptr().add(next_hashes[1] as usize) as *const u8);

            // Hash3: 2-way check for length 3 matches
            let h3 = hash3 * BT_MATCHFINDER_HASH3_WAYS;
            let cur_node_0 = *self.hash3_tab.get_unchecked(h3) as i32;
            *self.hash3_tab.get_unchecked_mut(h3) = cur_pos as i16;
            let cur_node_1 = *self.hash3_tab.get_unchecked(h3 + 1) as i32;
            *self.hash3_tab.get_unchecked_mut(h3 + 1) = cur_node_0 as i16;

            if RECORD_MATCHES && cur_node_0 > cutoff {
                let seq3 = load_u32_le_ptr(input_ptr, in_next) & 0xFFFFFF;
                let match0_pos = (in_base_offset as isize + cur_node_0 as isize) as usize;
                if seq3 == load_u32_le_ptr(input_ptr, match0_pos) & 0xFFFFFF {
                    *matches_ptr.add(match_count) = LzMatch {
                        length: 3,
                        offset: (in_next - match0_pos) as u16,
                    };
                    match_count += 1;
                } else if cur_node_1 > cutoff {
                    let match1_pos = (in_base_offset as isize + cur_node_1 as isize) as usize;
                    if seq3 == load_u32_le_ptr(input_ptr, match1_pos) & 0xFFFFFF {
                        *matches_ptr.add(match_count) = LzMatch {
                            length: 3,
                            offset: (in_next - match1_pos) as u16,
                        };
                        match_count += 1;
                    }
                }
            }

            // Hash4: binary tree for length 4+ matches
            let mut cur_node = *self.hash4_tab.get_unchecked(hash4) as i32;
            *self.hash4_tab.get_unchecked_mut(hash4) = cur_pos as i16;

            let mut pending_lt_idx = Self::left_child_idx(cur_pos);
            let mut pending_gt_idx = Self::right_child_idx(cur_pos);

            let child_ptr = self.child_tab.as_mut_ptr();

            if cur_node <= cutoff {
                *child_ptr.add(pending_lt_idx) = MATCHFINDER_INITVAL;
                *child_ptr.add(pending_gt_idx) = MATCHFINDER_INITVAL;
                return match_count;
            }

            let mut best_lt_len = 0u32;
            let mut best_gt_len = 0u32;
            let mut len = 0u32;

            loop {
                let match_pos = (in_base_offset as isize + cur_node as isize) as usize;

                if *input_ptr.add(match_pos + len as usize)
                    == *input_ptr.add(in_next + len as usize)
                {
                    len = lz_extend_raw(
                        input_ptr.add(in_next),
                        input_ptr.add(match_pos),
                        len + 1,
                        max_len,
                    );
                    if !RECORD_MATCHES || len > best_len {
                        if RECORD_MATCHES {
                            best_len = len;
                            *matches_ptr.add(match_count) = LzMatch {
                                length: len as u16,
                                offset: (in_next - match_pos) as u16,
                            };
                            match_count += 1;
                        }
                        if len >= nice_len {
                            *child_ptr.add(pending_lt_idx) =
                                *child_ptr.add(Self::left_child_idx(cur_node));
                            *child_ptr.add(pending_gt_idx) =
                                *child_ptr.add(Self::right_child_idx(cur_node));
                            return match_count;
                        }
                    }
                }

                if *input_ptr.add(match_pos + len as usize) < *input_ptr.add(in_next + len as usize)
                {
                    *child_ptr.add(pending_lt_idx) = cur_node as i16;
                    pending_lt_idx = Self::right_child_idx(cur_node);
                    cur_node = *child_ptr.add(pending_lt_idx) as i32;
                    best_lt_len = len;
                    if best_gt_len < len {
                        len = best_gt_len;
                    }
                } else {
                    *child_ptr.add(pending_gt_idx) = cur_node as i16;
                    pending_gt_idx = Self::left_child_idx(cur_node);
                    cur_node = *child_ptr.add(pending_gt_idx) as i32;
                    best_gt_len = len;
                    if best_lt_len < len {
                        len = best_lt_len;
                    }
                }

                depth_remaining -= 1;
                if cur_node <= cutoff || depth_remaining == 0 {
                    *child_ptr.add(pending_lt_idx) = MATCHFINDER_INITVAL;
                    *child_ptr.add(pending_gt_idx) = MATCHFINDER_INITVAL;
                    return match_count;
                }
            }
        } // unsafe
    }
}

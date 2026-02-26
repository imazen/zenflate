//! Full-optimal (Zopfli-style) DEFLATE compression.
//!
//! Ported from zenzop's squeeze/hash/cache/lz77/blocksplitter modules.
//! Uses a Zopfli-style forward DP parser with iterative cost model refinement,
//! then encodes blocks through zenflate's existing Huffman/precode encoder.

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, vec, vec::Vec};

use core::cmp;

use crate::CompressionError;
use crate::constants::*;

use super::bitstream::OutputBitstream;
use super::block::{
    BlockOutput, DeflateCodes, DeflateFreqs, LENGTH_SLOT, block_cost_best, flush_block_best,
    get_offset_slot, make_huffman_codes_best,
};
use super::huffman::{make_huffman_code_optimal, optimize_huffman_for_rle};
use super::katajainen::HuffmanScratch;
use super::sequences::Sequence;

// ---- Constants ----

const WINDOW_SIZE: usize = 32768;
const WINDOW_MASK: usize = WINDOW_SIZE - 1;
const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const MAX_CHAIN_HITS: usize = 8192;
const CACHE_LENGTH: usize = 8;
const NUM_LL: usize = 288;
const NUM_D: usize = 32;

const HASH_SHIFT: u32 = 5;
const HASH_MASK: u16 = 32767;
const HASH_NONE: u16 = u16::MAX;

// ---- ZopfliHash ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum WhichHash {
    Hash1,
    Hash2,
}

#[derive(Clone)]
struct HashChain {
    head: Box<[u16; 65536]>,
    prev: Box<[u16; WINDOW_SIZE]>,
    hashval: Box<[u16; WINDOW_SIZE]>,
    val: u16,
}

impl HashChain {
    fn new() -> Self {
        let mut prev: Box<[u16; WINDOW_SIZE]> = vec![0u16; WINDOW_SIZE]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!());
        for (i, p) in prev.iter_mut().enumerate() {
            *p = i as u16;
        }
        Self {
            head: vec![HASH_NONE; 65536]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
            prev,
            hashval: vec![HASH_NONE; WINDOW_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
            val: 0,
        }
    }

    fn reset(&mut self) {
        self.head.fill(HASH_NONE);
        for (i, p) in self.prev.iter_mut().enumerate() {
            *p = i as u16;
        }
        self.hashval.fill(HASH_NONE);
        self.val = 0;
    }

    fn update(&mut self, hpos: usize) {
        let hashval = self.val;
        let index = self.val as usize;
        let head_index = self.head[index];
        let prev = if head_index != HASH_NONE && self.hashval[head_index as usize] == self.val {
            head_index
        } else {
            hpos as u16
        };
        self.prev[hpos] = prev;
        self.hashval[hpos] = hashval;
        self.head[index] = hpos as u16;
    }
}

#[derive(Clone)]
struct ZopfliHash {
    hash1: HashChain,
    hash2: HashChain,
    same: Box<[u16; WINDOW_SIZE]>,
}

impl ZopfliHash {
    fn new() -> Box<Self> {
        Box::new(Self {
            hash1: HashChain::new(),
            hash2: HashChain::new(),
            same: vec![0u16; WINDOW_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap_or_else(|_| unreachable!()),
        })
    }

    fn reset(&mut self) {
        self.hash1.reset();
        self.hash2.reset();
        self.same.fill(0);
    }

    fn warmup(&mut self, arr: &[u8], pos: usize, end: usize) {
        self.update_val(arr[pos]);
        if pos + 1 < end {
            self.update_val(arr[pos + 1]);
        }
    }

    fn update_val(&mut self, c: u8) {
        self.hash1.val = ((self.hash1.val << HASH_SHIFT) ^ u16::from(c)) & HASH_MASK;
    }

    fn update(&mut self, array: &[u8], pos: usize) {
        let hash_value = array.get(pos + MIN_MATCH - 1).copied().unwrap_or(0);
        self.update_val(hash_value);

        let hpos = pos & WINDOW_MASK;
        self.hash1.update(hpos);

        // Update "same" (run-length of identical bytes).
        let mut amount: u16 = 0;
        let same = self.same[pos.wrapping_sub(1) & WINDOW_MASK];
        if same > 1 {
            amount = same - 1;
        }

        let array_pos = array[pos];
        let start = pos + amount as usize + 1;
        let scan_end = cmp::min(pos + u16::MAX as usize + 1, array.len());
        if start < scan_end {
            for &byte in &array[start..scan_end] {
                if byte != array_pos {
                    break;
                }
                amount += 1;
            }
        }

        self.same[hpos] = amount;
        self.hash2.val = (amount.wrapping_sub(MIN_MATCH as u16) & 255) ^ self.hash1.val;
        self.hash2.update(hpos);
    }

    fn prev_at(&self, index: usize, which: WhichHash) -> usize {
        (match which {
            WhichHash::Hash1 => self.hash1.prev[index],
            WhichHash::Hash2 => self.hash2.prev[index],
        }) as usize
            & WINDOW_MASK
    }

    fn hash_val_at(&self, index: usize, which: WhichHash) -> i32 {
        let hashval = match which {
            WhichHash::Hash1 => self.hash1.hashval[index],
            WhichHash::Hash2 => self.hash2.hashval[index],
        };
        if hashval == HASH_NONE {
            -1
        } else {
            hashval as i32
        }
    }

    fn val(&self, which: WhichHash) -> u16 {
        match which {
            WhichHash::Hash1 => self.hash1.val,
            WhichHash::Hash2 => self.hash2.val,
        }
    }
}

// ---- Match Cache ----

struct MatchCache {
    length: Vec<u16>,
    dist: Vec<u16>,
    sublen: Vec<u8>,
    /// True if all positions have complete sublen data in the cache.
    /// When true, subsequent iterations never need `find_longest_match_loop`
    /// and can safely skip hash chain updates.
    sublen_complete: bool,
}

impl MatchCache {
    fn new(blocksize: usize) -> Self {
        Self {
            length: vec![1; blocksize],
            dist: vec![0; blocksize],
            sublen: vec![0; CACHE_LENGTH * blocksize * 3],
            sublen_complete: true, // Assumed true until a store_sublen overflows
        }
    }

    fn is_sublen_complete(&self) -> bool {
        self.sublen_complete
    }

    fn max_sublen(&self, pos: usize) -> u32 {
        let start = CACHE_LENGTH * pos * 3;
        if self.sublen[start + 1] == 0 && self.sublen[start + 2] == 0 {
            return 0;
        }
        u32::from(self.sublen[start + ((CACHE_LENGTH - 1) * 3)]) + 3
    }

    fn store_sublen(&mut self, sublen: &[u16], pos: usize, length: usize) {
        if length < 3 {
            return;
        }
        let start = CACHE_LENGTH * pos * 3;
        let mut i = 3;
        let mut j = 0;
        let mut bestlength = 0;
        while i <= length {
            if i == length || sublen[i] != sublen[i + 1] {
                self.sublen[start + (j * 3)] = (i - 3) as u8;
                self.sublen[start + (j * 3 + 1)] = sublen[i].wrapping_rem(256) as u8;
                self.sublen[start + (j * 3 + 2)] = (sublen[i] >> 8).wrapping_rem(256) as u8;
                bestlength = i as u32;
                j += 1;
                if j >= CACHE_LENGTH {
                    break;
                }
            }
            i += 1;
        }
        if j < CACHE_LENGTH {
            self.sublen[start + ((CACHE_LENGTH - 1) * 3)] = (bestlength - 3) as u8;
        } else {
            self.sublen_complete = false;
        }
    }

    fn fetch_sublen(&self, pos: usize, length: usize, sublen: &mut [u16]) {
        if length < 3 {
            return;
        }
        let start = CACHE_LENGTH * pos * 3;
        let maxlength = self.max_sublen(pos) as usize;
        let mut prevlength = 0;
        for j in 0..CACHE_LENGTH {
            let length = self.sublen[start + (j * 3)] as usize + 3;
            let dist = u16::from(self.sublen[start + (j * 3 + 1)])
                + 256 * u16::from(self.sublen[start + (j * 3 + 2)]);
            let mut i = prevlength;
            while i <= length {
                sublen[i] = dist;
                i += 1;
            }
            if length == maxlength {
                break;
            }
            prevlength = length + 1;
        }
    }

    fn try_get(
        &self,
        pos: usize,
        mut limit: usize,
        sublen: &mut Option<&mut [u16]>,
        blockstart: usize,
    ) -> LongestMatch {
        let mut longest_match = LongestMatch::new(limit);
        let lmcpos = pos - blockstart;
        let length_lmcpos = self.length[lmcpos];
        let dist_lmcpos = self.dist[lmcpos];
        let cache_available = length_lmcpos == 0 || dist_lmcpos != 0;
        let max_sublen = self.max_sublen(lmcpos);
        let limit_ok = limit == MAX_MATCH
            || length_lmcpos <= limit as u16
            || (sublen.is_some() && max_sublen >= limit as u32);

        if limit_ok && cache_available {
            if sublen.is_none() || u32::from(length_lmcpos) <= max_sublen {
                let length = cmp::min(length_lmcpos, limit as u16);
                let distance;
                if let Some(ref mut subl) = *sublen {
                    self.fetch_sublen(lmcpos, length as usize, subl);
                    distance = subl[length as usize];
                    if limit == MAX_MATCH && length >= MIN_MATCH as u16 {
                        debug_assert_eq!(subl[length as usize], dist_lmcpos);
                    }
                } else {
                    distance = dist_lmcpos;
                }
                longest_match.distance = distance;
                longest_match.length = length;
                longest_match.from_cache = true;
                return longest_match;
            }
            limit = length_lmcpos as usize;
            longest_match.limit = limit;
        }
        longest_match
    }

    fn store(
        &mut self,
        pos: usize,
        _limit: usize,
        sublen: &mut Option<&mut [u16]>,
        distance: u16,
        length: u16,
        blockstart: usize,
    ) {
        if let Some(ref mut subl) = *sublen {
            let lmcpos = pos - blockstart;
            let cache_available = self.length[lmcpos] == 0 || self.dist[lmcpos] != 0;
            if !cache_available {
                if length < MIN_MATCH as u16 {
                    self.dist[lmcpos] = 0;
                    self.length[lmcpos] = 0;
                } else {
                    self.dist[lmcpos] = distance;
                    self.length[lmcpos] = length;
                }
                self.store_sublen(subl, lmcpos, length as usize);
            }
        }
    }
}

// ---- Match Finding ----

struct LongestMatch {
    distance: u16,
    length: u16,
    from_cache: bool,
    limit: usize,
}

impl LongestMatch {
    const fn new(limit: usize) -> Self {
        Self {
            distance: 0,
            length: 0,
            from_cache: false,
            limit,
        }
    }
}

fn get_match(scan_arr: &[u8], match_arr: &[u8]) -> usize {
    let max_prefix_len = cmp::min(scan_arr.len(), match_arr.len());
    let mut i = 0;
    const CHUNK_SIZE: usize = core::mem::size_of::<u128>();
    while i + CHUNK_SIZE < max_prefix_len && i + CHUNK_SIZE <= usize::MAX - CHUNK_SIZE {
        let scan_chunk = u128::from_le_bytes(scan_arr[i..i + CHUNK_SIZE].try_into().unwrap());
        let match_chunk = u128::from_le_bytes(match_arr[i..i + CHUNK_SIZE].try_into().unwrap());
        let bit_diff_mask = scan_chunk ^ match_chunk;
        if bit_diff_mask != 0 {
            return i + bit_diff_mask.trailing_zeros() as usize / 8;
        }
        i += CHUNK_SIZE;
    }
    for j in i..max_prefix_len {
        if scan_arr[j] != match_arr[j] {
            return j;
        }
    }
    max_prefix_len
}

#[allow(clippy::too_many_arguments)]
fn find_longest_match(
    lmc: &mut MatchCache,
    h: &ZopfliHash,
    array: &[u8],
    pos: usize,
    size: usize,
    blockstart: usize,
    limit: usize,
    sublen: &mut Option<&mut [u16]>,
) -> LongestMatch {
    let mut longest_match = lmc.try_get(pos, limit, sublen, blockstart);
    if longest_match.from_cache {
        return longest_match;
    }
    let mut limit = longest_match.limit;
    if size - pos < MIN_MATCH {
        longest_match.distance = 0;
        longest_match.length = 0;
        return longest_match;
    }
    if pos + limit > size {
        limit = size - pos;
    }
    let (bestlength, bestdist) = find_longest_match_loop(h, array, pos, size, limit, sublen);
    lmc.store(pos, limit, sublen, bestdist, bestlength, blockstart);
    longest_match.distance = bestdist;
    longest_match.length = bestlength;
    longest_match
}

fn find_longest_match_loop(
    h: &ZopfliHash,
    array: &[u8],
    pos: usize,
    size: usize,
    limit: usize,
    sublen: &mut Option<&mut [u16]>,
) -> (u16, u16) {
    let mut which_hash = WhichHash::Hash1;
    let hpos = pos & WINDOW_MASK;
    let mut pp = hpos;
    let mut p = h.prev_at(pp, which_hash);
    let mut dist = if p < pp { pp - p } else { WINDOW_SIZE - p + pp };
    let mut bestlength = 1;
    let mut bestdist = 0;
    let mut chain_counter = MAX_CHAIN_HITS;
    let arrayend = pos + limit;

    while dist < WINDOW_SIZE && chain_counter > 0 {
        let mut currentlength = 0;
        if dist > 0 {
            let scan_offset = pos;
            let match_offset = pos - dist;
            if pos + bestlength >= size
                || array[scan_offset + bestlength] == array[match_offset + bestlength]
            {
                let same0 = h.same[pos & WINDOW_MASK];
                let mut so = scan_offset;
                let mut mo = match_offset;
                if same0 > 2 && array[so] == array[mo] {
                    let same1 = h.same[(pos - dist) & WINDOW_MASK];
                    let same = cmp::min(cmp::min(same0, same1), limit as u16) as usize;
                    so += same;
                    mo += same;
                }
                let matched = get_match(&array[so..arrayend], &array[mo..arrayend]);
                currentlength = matched + so - pos;
            }
            if currentlength > bestlength {
                if let Some(ref mut subl) = *sublen {
                    for sublength in subl.iter_mut().take(currentlength + 1).skip(bestlength + 1) {
                        *sublength = dist as u16;
                    }
                }
                bestdist = dist;
                bestlength = currentlength;
                if currentlength >= limit {
                    break;
                }
            }
        }

        if which_hash == WhichHash::Hash1
            && bestlength >= h.same[hpos] as usize
            && i32::from(h.val(WhichHash::Hash2)) == h.hash_val_at(p, WhichHash::Hash2)
        {
            which_hash = WhichHash::Hash2;
        }

        pp = p;
        p = h.prev_at(p, which_hash);
        if p == pp {
            break;
        }
        dist += if p < pp { pp - p } else { WINDOW_SIZE - p + pp };
        chain_counter -= 1;
    }
    debug_assert!(
        bestlength <= limit,
        "find_longest_match_loop: bestlength={bestlength} > limit={limit}"
    );
    (bestlength as u16, bestdist as u16)
}

// ---- LZ77 Store ----

#[derive(Clone, Copy)]
enum LitLen {
    Literal(u16),
    LengthDist(u16, u16),
}

impl LitLen {
    const fn size(&self) -> usize {
        match *self {
            Self::Literal(_) => 1,
            Self::LengthDist(len, _) => len as usize,
        }
    }
}

#[derive(Clone, Default)]
struct Lz77Store {
    litlens: Vec<LitLen>,
    pos: Vec<u32>,
}

impl Lz77Store {
    fn with_capacity(blocksize: usize) -> Self {
        let cap = blocksize / 2;
        Self {
            litlens: Vec::with_capacity(cap),
            pos: Vec::with_capacity(cap),
        }
    }

    fn reset(&mut self) {
        self.litlens.clear();
        self.pos.clear();
    }

    fn size(&self) -> usize {
        self.litlens.len()
    }

    /// Extract a sub-store covering lz77 indices `start..end`.
    fn sub_store(&self, start: usize, end: usize) -> Self {
        Self {
            litlens: self.litlens[start..end].to_vec(),
            pos: if self.pos.is_empty() {
                Vec::new()
            } else {
                self.pos[start..end].to_vec()
            },
        }
    }

    fn lit_len_dist(&mut self, length: u16, dist: u16, pos: usize) {
        let litlen = if dist == 0 {
            debug_assert!(
                (length as usize) < NUM_LL,
                "literal value out of range: {length}"
            );
            LitLen::Literal(length)
        } else {
            debug_assert!(
                length as usize >= MIN_MATCH && length as usize <= MAX_MATCH,
                "match length out of range: {length}"
            );
            LitLen::LengthDist(length, dist)
        };
        self.litlens.push(litlen);
        self.pos.push(pos as u32);
    }

    fn greedy(&mut self, in_data: &[u8], instart: usize, inend: usize) {
        if instart == inend {
            return;
        }
        let windowstart = instart.saturating_sub(WINDOW_SIZE);
        let mut h = ZopfliHash::new();
        let arr = &in_data[..inend];
        h.warmup(arr, windowstart, inend);
        for i in windowstart..instart {
            h.update(arr, i);
        }

        // Use NoCache equivalent (just pass through to hash chain search)
        let mut i = instart;
        let mut prev_length: u32 = 0;
        let mut prev_match: u32 = 0;
        let mut match_available = false;

        while i < inend {
            h.update(arr, i);
            let (leng, dist) = find_longest_match_no_cache(&h, arr, i, inend, MAX_MATCH);
            let lengthscore = get_length_score(i32::from(leng), i32::from(dist));
            let prevlengthscore = get_length_score(prev_length as i32, prev_match as i32);

            if match_available {
                match_available = false;
                if lengthscore > prevlengthscore + 1 {
                    self.lit_len_dist(u16::from(arr[i - 1]), 0, i - 1);
                    if (lengthscore as usize) >= MIN_MATCH && (leng as usize) < MAX_MATCH {
                        match_available = true;
                        prev_length = u32::from(leng);
                        prev_match = u32::from(dist);
                        i += 1;
                        continue;
                    }
                } else {
                    let leng = prev_length as u16;
                    let dist = prev_match as u16;
                    self.lit_len_dist(leng, dist, i - 1);
                    for _ in 2..leng {
                        i += 1;
                        if i < inend {
                            h.update(arr, i);
                        }
                    }
                    i += 1;
                    continue;
                }
            } else if (lengthscore as usize) >= MIN_MATCH && (leng as usize) < MAX_MATCH {
                match_available = true;
                prev_length = u32::from(leng);
                prev_match = u32::from(dist);
                i += 1;
                continue;
            }

            if (lengthscore as usize) >= MIN_MATCH {
                self.lit_len_dist(leng, dist, i);
                let step = leng;
                for _ in 1..step {
                    i += 1;
                    if i < inend {
                        h.update(arr, i);
                    }
                }
            } else {
                self.lit_len_dist(u16::from(arr[i]), 0, i);
            }
            i += 1;
        }
    }

    fn store_from_path(&mut self, in_data: &[u8], instart: usize, path: &[(u16, u16)]) {
        let mut pos = instart;
        for &(length, dist) in path.iter().rev() {
            if length >= MIN_MATCH as u16 {
                self.lit_len_dist(length, dist, pos);
            } else {
                self.lit_len_dist(u16::from(in_data[pos]), 0, pos);
            }
            pos += length as usize;
        }
    }
}

fn find_longest_match_no_cache(
    h: &ZopfliHash,
    array: &[u8],
    pos: usize,
    size: usize,
    limit: usize,
) -> (u16, u16) {
    if size - pos < MIN_MATCH {
        return (0, 0);
    }
    let limit = cmp::min(limit, size - pos);
    // Returns (length, dist)
    find_longest_match_loop(h, array, pos, size, limit, &mut None)
}

const fn get_length_score(length: i32, distance: i32) -> i32 {
    if distance > 1024 { length - 1 } else { length }
}

// ---- Symbol Tables ----

fn get_length_symbol(length: usize) -> usize {
    DEFLATE_FIRST_LEN_SYM as usize + LENGTH_SLOT[length] as usize
}

fn get_dist_symbol(dist: u16) -> usize {
    get_offset_slot(dist as u32) as usize
}

fn get_length_symbol_extra_bits(sym: usize) -> u32 {
    DEFLATE_LENGTH_EXTRA_BITS[sym - DEFLATE_FIRST_LEN_SYM as usize] as u32
}

fn get_dist_symbol_extra_bits(dsym: usize) -> u32 {
    DEFLATE_OFFSET_EXTRA_BITS[dsym] as u32
}

// ---- Cost Model + SymbolStats ----

#[derive(Copy, Clone)]
struct SymbolStats {
    litlens: [usize; NUM_LL],
    dists: [usize; NUM_D],
    ll_symbols: [f64; NUM_LL],
    d_symbols: [f64; NUM_D],
}

impl Default for SymbolStats {
    fn default() -> Self {
        Self {
            litlens: [0; NUM_LL],
            dists: [0; NUM_D],
            ll_symbols: [0.0; NUM_LL],
            d_symbols: [0.0; NUM_D],
        }
    }
}

impl SymbolStats {
    fn get_statistics(&mut self, store: &Lz77Store) {
        for &litlen in &store.litlens {
            match litlen {
                LitLen::Literal(lit) => self.litlens[lit as usize] += 1,
                LitLen::LengthDist(len, dist) => {
                    self.litlens[get_length_symbol(len as usize)] += 1;
                    self.dists[get_dist_symbol(dist)] += 1;
                }
            }
        }
        self.litlens[256] = 1;
        self.calculate_entropy();
    }

    fn calculate_entropy(&mut self) {
        fn calculate_and_store(count: &[usize], bitlengths: &mut [f64]) {
            let n = count.len();
            let sum: usize = count.iter().sum();
            let log2sum = (if sum == 0 { n } else { sum } as f64).log2();
            for i in 0..n {
                if count[i] == 0 {
                    bitlengths[i] = log2sum;
                } else {
                    bitlengths[i] = log2sum - (count[i] as f64).log2();
                }
            }
        }
        calculate_and_store(&self.litlens, &mut self.ll_symbols);
        calculate_and_store(&self.dists, &mut self.d_symbols);
    }

    /// Set frequencies from pre-computed counts and calculate entropy.
    fn set_frequencies(&mut self, ll_counts: &[usize; NUM_LL], d_counts: &[usize; NUM_D]) {
        self.litlens = *ll_counts;
        self.dists = *d_counts;
        self.litlens[256] = 1; // End symbol
        self.calculate_entropy();
    }

    fn randomize_stat_freqs(&mut self, state: &mut RanState) {
        fn randomize_freqs(freqs: &mut [usize], state: &mut RanState) {
            let n = freqs.len();
            for i in 0..n {
                if (state.random_marsaglia() >> 4).is_multiple_of(3) {
                    let index = state.random_marsaglia() as usize % n;
                    freqs[i] = freqs[index];
                }
            }
        }
        randomize_freqs(&mut self.litlens, state);
        randomize_freqs(&mut self.dists, state);
        self.litlens[256] = 1;
    }

    /// Build Huffman code lengths and use them as cost model.
    fn calculate_huffman_costs(&mut self, beststats: &SymbolStats, scratch: &mut HuffmanScratch) {
        let mut ll_counts = beststats.litlens;
        let mut d_counts = beststats.dists;
        optimize_huffman_for_rle_usize(&mut ll_counts);
        optimize_huffman_for_rle_usize(&mut d_counts);

        // Build code lengths via katajainen's optimal bounded package merge
        let ll_freqs: Vec<u32> = ll_counts.iter().map(|&c| c as u32).collect();
        let d_freqs: Vec<u32> = d_counts.iter().map(|&c| c as u32).collect();

        let mut ll_lens = [0u8; NUM_LL];
        let mut ll_cw = [0u32; NUM_LL];
        make_huffman_code_optimal(NUM_LL, 15, &ll_freqs, &mut ll_lens, &mut ll_cw, scratch);

        let mut d_lens = [0u8; NUM_D];
        let mut d_cw = [0u32; NUM_D];
        make_huffman_code_optimal(NUM_D, 15, &d_freqs, &mut d_lens, &mut d_cw, scratch);

        for (i, &len) in ll_lens.iter().enumerate() {
            self.ll_symbols[i] = f64::from(len);
        }
        for (i, &len) in d_lens.iter().enumerate() {
            self.d_symbols[i] = f64::from(len);
        }
    }
}

/// Zopfli-style RLE smoothing on usize arrays.
fn optimize_huffman_for_rle_usize(counts: &mut [usize]) {
    // Convert to u32, apply zenflate's optimize_huffman_for_rle, convert back
    let mut u32_counts: Vec<u32> = counts.iter().map(|&c| c as u32).collect();
    optimize_huffman_for_rle(&mut u32_counts);
    for (dst, &src) in counts.iter_mut().zip(u32_counts.iter()) {
        *dst = src as usize;
    }
}

struct CostModel {
    ll_literal: [f32; 256],
    ll_length: [f32; MAX_MATCH + 1],
    d_cost: [f32; NUM_D],
}

impl CostModel {
    fn from_stats(stats: &SymbolStats) -> Self {
        let mut ll_literal = [0.0f32; 256];
        for (i, cost) in ll_literal.iter_mut().enumerate() {
            *cost = stats.ll_symbols[i] as f32;
        }

        let mut ll_length = [0.0f32; MAX_MATCH + 1];
        for (i, cost) in ll_length.iter_mut().enumerate().skip(3) {
            let lsym = get_length_symbol(i);
            *cost = (stats.ll_symbols[lsym] + f64::from(get_length_symbol_extra_bits(lsym))) as f32;
        }

        let mut d_cost = [0.0f32; NUM_D];
        for (dsym, cost) in d_cost.iter_mut().enumerate().take(30) {
            *cost = (stats.d_symbols[dsym] + f64::from(get_dist_symbol_extra_bits(dsym))) as f32;
        }

        Self {
            ll_literal,
            ll_length,
            d_cost,
        }
    }

    #[inline(always)]
    fn cost(&self, litlen: usize, dist: u16) -> f64 {
        if dist == 0 {
            f64::from(self.ll_literal[litlen])
        } else {
            f64::from(self.ll_length[litlen]) + f64::from(self.d_cost[get_dist_symbol(dist)])
        }
    }
}

fn add_weighed_stat_freqs(
    stats1: &SymbolStats,
    w1: f64,
    stats2: &SymbolStats,
    w2: f64,
) -> SymbolStats {
    let mut result = SymbolStats::default();
    for i in 0..NUM_LL {
        result.litlens[i] =
            (stats1.litlens[i] as f64 * w1 + stats2.litlens[i] as f64 * w2) as usize;
    }
    for i in 0..NUM_D {
        result.dists[i] = (stats1.dists[i] as f64 * w1 + stats2.dists[i] as f64 * w2) as usize;
    }
    result.litlens[256] = 1;
    result
}

// ---- RNG ----

#[derive(Default)]
struct RanState {
    m_w: u32,
    m_z: u32,
}

impl RanState {
    fn random_marsaglia(&mut self) -> u32 {
        self.m_z = 36969u32
            .wrapping_mul(self.m_z & 65535)
            .wrapping_add(self.m_z >> 16);
        self.m_w = 18000u32
            .wrapping_mul(self.m_w & 65535)
            .wrapping_add(self.m_w >> 16);
        (self.m_z << 16).wrapping_add(self.m_w)
    }
}

// ---- Forward DP ----

fn get_cost_model_min_cost(cost_model: &CostModel) -> f64 {
    const DSYMBOLS: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    let mut bestlength = 3;
    let mut mincost = f64::INFINITY;
    for i in 3..259 {
        let c = cost_model.cost(i, 1);
        if c < mincost {
            bestlength = i;
            mincost = c;
        }
    }
    let mut bestdist = 1u16;
    mincost = f64::INFINITY;
    for dsym in DSYMBOLS {
        let c = cost_model.cost(3, dsym);
        if c < mincost {
            bestdist = dsym;
            mincost = c;
        }
    }
    cost_model.cost(bestlength, bestdist)
}

#[allow(clippy::too_many_arguments)]
fn get_best_lengths(
    lmc: &mut MatchCache,
    in_data: &[u8],
    instart: usize,
    inend: usize,
    cost_model: &CostModel,
    h: &mut ZopfliHash,
    costs: &mut Vec<f32>,
    length_array: &mut Vec<u16>,
    dist_array: &mut Vec<u16>,
    sublen: &mut Vec<u16>,
    skip_hash: bool,
) -> f64 {
    let blocksize = inend - instart;
    length_array.clear();
    length_array.resize(blocksize + 1, 0);
    dist_array.clear();
    dist_array.resize(blocksize + 1, 0);
    if instart == inend {
        return 0.0;
    }
    let windowstart = instart.saturating_sub(WINDOW_SIZE);

    let arr = &in_data[..inend];
    if !skip_hash {
        // Only iteration 0 needs full hash chain setup.
        // On cached iterations (skip_hash=true), all match lookups return from
        // cache — neither hash chains nor the `same` array are needed.
        h.reset();
        h.warmup(arr, windowstart, inend);
        for i in windowstart..instart {
            h.update(arr, i);
        }
    }

    costs.resize(blocksize + 1, 0.0);
    for cost in costs.iter_mut().take(blocksize + 1).skip(1) {
        *cost = f32::INFINITY;
    }
    costs[0] = 0.0;

    let mut i = instart;
    sublen.resize(MAX_MATCH + 1, 0);
    let mincost = get_cost_model_min_cost(cost_model);

    while i < inend {
        let mut j = i - instart;
        if !skip_hash {
            h.update(arr, i);
        }

        // Skip optimization for long repetitions.
        // Skip this shortcut on cached iterations — `same` is not maintained.
        if !skip_hash
            && h.same[i & WINDOW_MASK] > MAX_MATCH as u16 * 2
            && i > instart + MAX_MATCH + 1
            && i + MAX_MATCH * 2 + 1 < inend
            && h.same[(i - MAX_MATCH) & WINDOW_MASK] > MAX_MATCH as u16
        {
            let symbolcost = cost_model.cost(MAX_MATCH, 1);
            for _ in 0..MAX_MATCH {
                costs[j + MAX_MATCH] = costs[j] + symbolcost as f32;
                length_array[j + MAX_MATCH] = MAX_MATCH as u16;
                dist_array[j + MAX_MATCH] = 1;
                i += 1;
                j += 1;
                h.update(arr, i);
            }
        }

        let longest_match = find_longest_match(
            lmc,
            h,
            arr,
            i,
            inend,
            instart,
            MAX_MATCH,
            &mut Some(sublen.as_mut_slice()),
        );
        let leng = longest_match.length;

        // Literal.
        if i < inend {
            let new_cost = cost_model.cost(arr[i] as usize, 0) + f64::from(costs[j]);
            if new_cost < f64::from(costs[j + 1]) {
                costs[j + 1] = new_cost as f32;
                length_array[j + 1] = 1;
                dist_array[j + 1] = 0;
            }
        }

        // Lengths.
        let kend = cmp::min(leng as usize, inend - i);
        let mincostaddcostj = mincost + f64::from(costs[j]);

        for (k, &sublength) in sublen.iter().enumerate().take(kend + 1).skip(3) {
            if f64::from(costs[j + k]) <= mincostaddcostj {
                continue;
            }
            let new_cost = cost_model.cost(k, sublength) + f64::from(costs[j]);
            if new_cost < f64::from(costs[j + k]) {
                costs[j + k] = new_cost as f32;
                length_array[j + k] = k as u16;
                dist_array[j + k] = sublength;
            }
        }
        i += 1;
    }

    f64::from(costs[blocksize])
}

fn trace(size: usize, length_array: &[u16], dist_array: &[u16], path: &mut Vec<(u16, u16)>) {
    path.clear();
    if size == 0 {
        return;
    }
    let mut index = size;
    while index > 0 {
        let lai = length_array[index];
        let dai = dist_array[index];
        path.push((lai, dai));
        index -= lai as usize;
    }
}

/// Compute symbol frequencies directly from a trace path, without building an Lz77Store.
/// The path is in reverse order (end to start).
fn compute_frequencies_from_path(
    in_data: &[u8],
    instart: usize,
    path: &[(u16, u16)],
) -> DeflateFreqs {
    let mut freqs = DeflateFreqs::default();
    let mut pos = instart;
    for &(length, dist) in path.iter().rev() {
        if length >= MIN_MATCH as u16 {
            freqs.litlen[get_length_symbol(length as usize)] += 1;
            freqs.offset[get_dist_symbol(dist)] += 1;
        } else {
            freqs.litlen[in_data[pos] as usize] += 1;
        }
        pos += length as usize;
    }
    freqs.litlen[256] += 1; // End symbol
    freqs
}

// ---- Block Cost Estimation ----

/// Compute accurate dynamic block cost from LZ77 store histogram.
///
/// Uses multi-strategy Huffman optimization (3 RLE strategies + max-bits sweep)
/// with exhaustive precode flag search for accurate tree header cost.
/// This matches the quality of block encoding in `flush_block_best`, ensuring
/// block split decisions use the same cost model as final output.
fn calculate_block_size_dynamic(
    store: &Lz77Store,
    lstart: usize,
    lend: usize,
    scratch: &mut HuffmanScratch,
) -> f64 {
    // Build histograms
    let mut freqs = DeflateFreqs::default();
    for &litlen in &store.litlens[lstart..lend] {
        match litlen {
            LitLen::Literal(lit) => freqs.litlen[lit as usize] += 1,
            LitLen::LengthDist(len, dist) => {
                freqs.litlen[get_length_symbol(len as usize)] += 1;
                freqs.offset[get_dist_symbol(dist)] += 1;
            }
        }
    }
    freqs.litlen[256] += 1; // end symbol

    // Use the same multi-strategy cost evaluation as final block encoding
    f64::from(block_cost_best(&freqs, scratch))
}

// ---- Block Splitter ----

fn find_minimum<F: FnMut(usize) -> f64>(mut f: F, start: usize, end: usize) -> (usize, f64) {
    if end - start < 1024 {
        let mut best = f64::INFINITY;
        let mut result = start;
        for i in start..end {
            let v = f(i);
            if v < best {
                best = v;
                result = i;
            }
        }
        (result, best)
    } else {
        let mut start = start;
        let mut end = end;
        const NUM: usize = 9;
        let mut p = [0; NUM];
        let mut vp = [0.0; NUM];
        let mut lastbest = f64::INFINITY;
        let mut pos = start;

        while end - start > NUM {
            let mut besti = 0;
            let mut best = f64::INFINITY;
            let multiplier = (end - start) / (NUM + 1);
            for i in 0..NUM {
                p[i] = start + (i + 1) * multiplier;
                vp[i] = f(p[i]);
                if vp[i] < best {
                    best = vp[i];
                    besti = i;
                }
            }
            if best > lastbest {
                break;
            }
            start = if besti == 0 { start } else { p[besti - 1] };
            end = if besti == NUM - 1 { end } else { p[besti + 1] };
            pos = p[besti];
            lastbest = best;
        }
        (pos, lastbest)
    }
}

fn blocksplit_lz77(lz77: &Lz77Store, maxblocks: u16, splitpoints: &mut Vec<usize>) {
    if lz77.size() < 10 {
        return;
    }
    let mut numblocks = 1u32;
    let mut done = vec![0u8; lz77.size()];
    let mut lstart = 0;
    let mut lend = lz77.size();
    let mut scratch = HuffmanScratch::new();

    while maxblocks != 0 && numblocks < u32::from(maxblocks) {
        let (llpos, splitcost) = find_minimum(
            |i| {
                calculate_block_size_dynamic(lz77, lstart, i, &mut scratch)
                    + calculate_block_size_dynamic(lz77, i, lend, &mut scratch)
            },
            lstart + 1,
            lend,
        );
        let origcost = calculate_block_size_dynamic(lz77, lstart, lend, &mut scratch);

        if splitcost > origcost || llpos == lstart + 1 || llpos == lend {
            done[lstart] = 1;
        } else {
            splitpoints.push(llpos);
            splitpoints.sort_unstable();
            numblocks += 1;
        }

        // Find largest splittable block
        let mut longest = 0;
        let mut found = false;
        let mut last = 0;
        for &item in splitpoints.iter() {
            if done[last] == 0 && item - last > longest {
                lstart = last;
                lend = item;
                longest = item - last;
                found = true;
            }
            last = item;
        }
        let end = lz77.size() - 1;
        if done[last] == 0 && end - last > longest {
            lstart = last;
            lend = end;
            found = true;
        }
        if !found || lend - lstart < 10 {
            break;
        }
    }
}

fn blocksplit(
    in_data: &[u8],
    instart: usize,
    inend: usize,
    maxblocks: u16,
    splitpoints: &mut Vec<usize>,
) {
    splitpoints.clear();
    let mut store = Lz77Store::with_capacity(inend - instart);
    store.greedy(in_data, instart, inend);

    let mut lz77splitpoints = Vec::with_capacity(maxblocks as usize);
    blocksplit_lz77(&store, maxblocks, &mut lz77splitpoints);

    let nlz77points = lz77splitpoints.len();
    let mut pos = instart;
    if nlz77points > 0 {
        for (i, item) in store.litlens.iter().enumerate() {
            let length = item.size();
            if lz77splitpoints[splitpoints.len()] == i {
                splitpoints.push(pos);
                if splitpoints.len() == nlz77points {
                    break;
                }
            }
            pos += length;
        }
    }
}

// ---- Squeeze Loop ----

fn lz77_optimal(
    in_data: &[u8],
    instart: usize,
    inend: usize,
    iterations: u64,
    stop: &impl enough::Stop,
) -> Result<Lz77Store, CompressionError> {
    let blocksize = inend - instart;
    let mut lmc = MatchCache::new(blocksize);
    let mut currentstore = Lz77Store::with_capacity(blocksize);
    let mut outputstore = Lz77Store::default();
    let mut huff_scratch = HuffmanScratch::new();

    // Initial greedy seed
    currentstore.greedy(in_data, instart, inend);
    let mut stats = SymbolStats::default();
    stats.get_statistics(&currentstore);
    outputstore.clone_from(&currentstore);

    let mut bestcost =
        calculate_block_size_dynamic(&currentstore, 0, currentstore.size(), &mut huff_scratch);

    let mut h = ZopfliHash::new();
    let mut costs = Vec::with_capacity(inend - instart + 1);
    let mut length_array = Vec::new();
    let mut dist_array = Vec::new();
    let mut sublen = Vec::new();
    let mut path_buf = Vec::new();

    let mut beststats = SymbolStats::default();
    let mut lastcost = 0.0;

    let mut ran_state = RanState {
        m_w: (blocksize as u32).wrapping_mul(0x9E3779B9).wrapping_add(1),
        m_z: (blocksize as u32).wrapping_mul(0x9E3779B9).wrapping_add(2),
    };
    let mut lastrandomstep = u64::MAX;
    let mut diversification_attempts: u64 = 0;
    let mut checkpoint: Option<SymbolStats> = None;

    let mut current_iteration: u64 = 0;
    // Don't exit early due to stagnation — always run all requested iterations.
    // Diversification + checkpoint/restore handles stagnation recovery.
    // This matches zenzop's default of iterations_without_improvement = u64::MAX.
    let max_iterations_without_improvement = u64::MAX;

    let mut iterations_without_improvement: u64 = 0;

    loop {
        // Check cooperative cancellation
        match stop.check() {
            Ok(()) => {}
            Err(enough::StopReason::Cancelled) => {
                return Err(CompressionError::Stopped(enough::StopReason::Cancelled));
            }
            Err(_) => break, // Timeout: return best-so-far
        }

        // Enhanced: milestone RLE at iteration 29
        if current_iteration == 29 {
            stats.calculate_huffman_costs(&beststats, &mut huff_scratch);
        }

        let cost_model = CostModel::from_stats(&stats);
        // After the first iteration populates the match cache, skip hash chain
        // updates on subsequent iterations if the cache has complete sublen data.
        let skip_hash = current_iteration > 0 && lmc.is_sublen_complete();
        // Run DP forward pass + trace without building an Lz77Store.
        // Frequencies and block cost are computed directly from the path.
        get_best_lengths(
            &mut lmc,
            in_data,
            instart,
            inend,
            &cost_model,
            &mut h,
            &mut costs,
            &mut length_array,
            &mut dist_array,
            &mut sublen,
            skip_hash,
        );
        trace(inend - instart, &length_array, &dist_array, &mut path_buf);
        let freqs = compute_frequencies_from_path(in_data, instart, &path_buf);
        let cost = f64::from(block_cost_best(&freqs, &mut huff_scratch));

        if cost < bestcost {
            iterations_without_improvement = 0;
            // Build full store only on improvement (needed for block splitting later)
            outputstore.reset();
            outputstore.store_from_path(in_data, instart, &path_buf);
            beststats = stats;
            bestcost = cost;

            if lastrandomstep != u64::MAX && checkpoint.is_none() {
                checkpoint = Some(beststats);
            }
        } else {
            iterations_without_improvement += 1;
            if iterations_without_improvement >= max_iterations_without_improvement {
                break;
            }
        }

        current_iteration += 1;
        if current_iteration >= iterations {
            // Ultra post-processing pass
            if current_iteration > 4 {
                let mut ultra_stats = SymbolStats::default();
                ultra_stats.calculate_huffman_costs(&beststats, &mut huff_scratch);
                let cost_model = CostModel::from_stats(&ultra_stats);
                let ultra_skip_hash = lmc.is_sublen_complete();
                get_best_lengths(
                    &mut lmc,
                    in_data,
                    instart,
                    inend,
                    &cost_model,
                    &mut h,
                    &mut costs,
                    &mut length_array,
                    &mut dist_array,
                    &mut sublen,
                    ultra_skip_hash,
                );
                trace(inend - instart, &length_array, &dist_array, &mut path_buf);
                let ultra_freqs = compute_frequencies_from_path(in_data, instart, &path_buf);
                let ultra_cost = f64::from(block_cost_best(&ultra_freqs, &mut huff_scratch));
                if ultra_cost < bestcost {
                    outputstore.reset();
                    outputstore.store_from_path(in_data, instart, &path_buf);
                }
            }
            break;
        }

        let laststats = stats;
        // Convert DeflateFreqs (u32) to SymbolStats (usize) frequencies
        let mut ll_counts = [0usize; NUM_LL];
        let mut d_counts = [0usize; NUM_D];
        for (i, &c) in freqs.litlen.iter().enumerate() {
            ll_counts[i] = c as usize;
        }
        for (i, &c) in freqs.offset.iter().enumerate() {
            d_counts[i] = c as usize;
        }
        stats.set_frequencies(&ll_counts, &d_counts);

        if lastrandomstep != u64::MAX {
            stats = add_weighed_stat_freqs(&stats, 1.0, &laststats, 0.5);
            stats.calculate_entropy();
        }

        if current_iteration > 5 && (cost - lastcost).abs() < f64::EPSILON {
            if diversification_attempts < 3 {
                diversification_attempts += 1;
                stats = beststats;
                stats.randomize_stat_freqs(&mut ran_state);
                stats.calculate_entropy();
                lastrandomstep = current_iteration;
            } else if diversification_attempts >= 3 {
                if let Some(cp) = checkpoint.take() {
                    stats = cp;
                    stats.calculate_entropy();
                } else {
                    stats = beststats;
                    stats.randomize_stat_freqs(&mut ran_state);
                    stats.calculate_entropy();
                    lastrandomstep = current_iteration;
                }
            }
        }
        lastcost = cost;
    }
    Ok(outputstore)
}

// ---- Public entry point ----

/// Full-optimal state (heap-allocated to avoid stack overflow).
pub(crate) struct FullOptimalState {
    iterations: u64,
}

impl Clone for FullOptimalState {
    fn clone(&self) -> Self {
        Self {
            iterations: self.iterations,
        }
    }
}

impl FullOptimalState {
    pub fn new(iterations: u64) -> Box<Self> {
        Box::new(Self { iterations })
    }

    pub fn iterations(&self) -> u64 {
        self.iterations
    }
}

/// Compress a block using the full-optimal (Zopfli) parser.
///
/// Supports cooperative cancellation via the `stop` token. On timeout,
/// returns the best result found so far. On cancel, returns an error.
pub(crate) fn compress_full_optimal(
    os: &mut OutputBitstream<'_>,
    input: &[u8],
    iterations: u64,
    is_final: bool,
    stop: &impl enough::Stop,
) -> Result<(), CompressionError> {
    if input.is_empty() {
        return Ok(());
    }

    // Phase 1: Initial byte-based block splitting
    let maxblocks = 15u16;
    let mut byte_splitpoints = Vec::new();
    blocksplit(input, 0, input.len(), maxblocks, &mut byte_splitpoints);

    // Build block boundaries from byte split points
    let mut boundaries = Vec::with_capacity(byte_splitpoints.len() + 2);
    boundaries.push(0usize);
    for &sp in &byte_splitpoints {
        boundaries.push(sp);
    }
    boundaries.push(input.len());

    // Phase 2: LZ77-optimize each block and concatenate
    let mut combined_lz77 = Lz77Store::with_capacity(input.len());
    let mut lz77_splitpoints = Vec::with_capacity(byte_splitpoints.len());

    for window in boundaries.windows(2) {
        let block_start = window[0];
        let block_end = window[1];

        let store = lz77_optimal(input, block_start, block_end, iterations, stop)?;

        // Append to combined store
        for &litlen in &store.litlens {
            combined_lz77.litlens.push(litlen);
        }

        // Record split point (all but last block)
        if block_end < input.len() {
            lz77_splitpoints.push(combined_lz77.size());
        }
    }

    // Phase 3: Second block split attempt on the LZ77 data (matches zenzop)
    let npoints = byte_splitpoints.len();
    if npoints > 1 {
        let mut splitpoints2 = Vec::with_capacity(npoints);
        blocksplit_lz77(&combined_lz77, maxblocks, &mut splitpoints2);

        // Compare costs of both splits
        let mut scratch = HuffmanScratch::new();
        let cost1 = calculate_split_cost(&combined_lz77, &lz77_splitpoints, &mut scratch);
        let cost2 = calculate_split_cost(&combined_lz77, &splitpoints2, &mut scratch);

        if cost2 < cost1 {
            lz77_splitpoints = splitpoints2;
        }
    }

    // Phase 4: Flush blocks using the best split
    let mut block_ranges = Vec::with_capacity(lz77_splitpoints.len() + 1);
    let mut last = 0;
    for &sp in &lz77_splitpoints {
        block_ranges.push((last, sp));
        last = sp;
    }
    block_ranges.push((last, combined_lz77.size()));

    // We need to track byte offsets for each LZ77 block
    let mut byte_offset = 0usize;
    for (bi, &(lz_start, lz_end)) in block_ranges.iter().enumerate() {
        let is_final_block = is_final && bi == block_ranges.len() - 1;

        // Extract the sub-store for this block
        let sub_store = combined_lz77.sub_store(lz_start, lz_end);

        // Calculate byte length of this sub-store
        let block_byte_len: usize = sub_store.litlens.iter().map(|ll| ll.size()).sum();

        flush_lz77_block(os, &input[byte_offset..], 0, &sub_store, is_final_block);
        byte_offset += block_byte_len;
    }
    Ok(())
}

/// Calculate the total block cost for a given set of LZ77 split points.
fn calculate_split_cost(
    lz77: &Lz77Store,
    splitpoints: &[usize],
    scratch: &mut HuffmanScratch,
) -> f64 {
    let mut cost = 0.0;
    let mut last = 0;
    for &sp in splitpoints {
        cost += calculate_block_size_dynamic(lz77, last, sp, scratch);
        last = sp;
    }
    cost += calculate_block_size_dynamic(lz77, last, lz77.size(), scratch);
    cost
}

/// Convert an Lz77Store to zenflate's Sequence format and flush through the block encoder.
fn flush_lz77_block(
    os: &mut OutputBitstream<'_>,
    input: &[u8],
    block_start: usize,
    store: &Lz77Store,
    is_final_block: bool,
) {
    let block_data = &input[block_start..];
    let mut block_length = 0usize;

    // Build frequency tables
    let mut freqs = DeflateFreqs::default();
    for &litlen in &store.litlens {
        match litlen {
            LitLen::Literal(lit) => {
                freqs.litlen[lit as usize] += 1;
            }
            LitLen::LengthDist(len, dist) => {
                let len_slot = LENGTH_SLOT[len as usize] as usize;
                freqs.litlen[DEFLATE_FIRST_LEN_SYM as usize + len_slot] += 1;
                freqs.offset[get_offset_slot(dist as u32) as usize] += 1;
            }
        }
        block_length += litlen.size();
    }
    freqs.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;

    // Build sequences
    let mut sequences = Vec::with_capacity(store.size() + 1);
    let mut litrunlen = 0u32;
    for &litlen in &store.litlens {
        match litlen {
            LitLen::Literal(_) => {
                litrunlen += 1;
            }
            LitLen::LengthDist(len, dist) => {
                let offset_slot = get_offset_slot(dist as u32);
                let seq = Sequence {
                    litrunlen_and_length: litrunlen
                        | ((len as u32) << super::sequences::SEQ_LENGTH_SHIFT),
                    offset: dist,
                    offset_slot: offset_slot as u16,
                };
                sequences.push(seq);
                litrunlen = 0;
            }
        }
    }
    // Final sequence (end-of-block marker with remaining literals)
    sequences.push(Sequence {
        litrunlen_and_length: litrunlen,
        offset: 0,
        offset_slot: 0,
    });

    // Build Huffman codes with multi-strategy optimization
    let mut codes = DeflateCodes::default();
    make_huffman_codes_best(&freqs, &mut codes);

    // Also build static codes for comparison
    let mut static_freqs = DeflateFreqs::default();
    let mut static_codes = DeflateCodes::default();
    super::block::init_static_codes(&mut static_freqs, &mut static_codes);

    // Flush through zenflate's block encoder
    flush_block_best(
        os,
        block_data,
        block_length,
        BlockOutput::Sequences(&sequences),
        &freqs,
        &codes,
        &static_codes,
        is_final_block,
    );
}

//! PNG-tuned compression for [`CompressionLevel::png`](super::CompressionLevel::png)
//! efforts 2-9 (effort 1 is the ultra-fast encoder in `png_ultra`), and the
//! runs-only guard ([`RunsGuard`]) that efforts 10-18 add to the lazy parser.
//!
//! PNG encoders hand DEFLATE filtered scanlines: per-row residuals that are
//! mostly small literals, long runs of a repeated byte (flat regions after
//! the Sub/Up filters), and occasional long repeats (identical rows or tiles).
//! Short matches in this data often cost more bits than the literals they
//! replace, and a general matchfinder spends most of its time looking for them.
//!
//! The parser starts from the greedy and RLE parsers in
//! [fdeflate](https://github.com/image-rs/fdeflate) (MIT OR Apache-2.0, the
//! image-rs developers):
//!
//! - **Runs first.** Five equal bytes at the cursor become a distance-1
//!   match, extended backwards into the pending literals and forwards up to
//!   258 bytes, with no hash-table work inside the run (effort 2 stops here).
//! - **Hashed repeats** of at least 8 bytes (effort 3) or 5 bytes (4-9),
//!   from a single-entry table (3-5) or hash chains (6-9), extended
//!   backwards into the pending literal run.
//! - **Skip-ahead in incompressible stretches.** The step grows with the
//!   distance since the last match (`1 + gap >> skip_shift`).
//!
//! and adds, for ratio and monotonicity across efforts:
//!
//! - **A runs-only guard.** On flat-colour art, far LZ77 matches split runs
//!   and lengthen the distance-1 code, and the runs-only parse can be ~10%
//!   smaller. Hash efforts parse each block runs-only too and emit the
//!   cheaper parse; when runs-only loses clearly (photos), the comparison
//!   pauses for a few blocks.
//! - **A 256-byte cap on the skip-ahead step,** so one long matchless stretch
//!   can't carry the parser across a compressible region.
//! - **Fixed 128 KiB blocks**, so every effort splits the input identically
//!   and the guard compares like with like.
//!
//! Blocks are written by zenflate's regular block writer, which picks the
//! cheapest of dynamic Huffman, static Huffman and stored per block.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::bitstream::OutputBitstream;
use super::block::{
    DeflateCodes, DeflateFreqs, block_symbol_cost, choose_match, dynamic_header_bits, finish_block,
    finish_block_with_codes, make_huffman_codes,
};
use super::block_split::MIN_BLOCK_LENGTH;
use super::sequences::Sequence;
use super::{Compressor, STOP_CHECK_INTERVAL};
use crate::constants::{DEFLATE_END_OF_BLOCK, DEFLATE_MAX_MATCH_LEN};
use crate::error::CompressionError;
use crate::fast_bytes::load_u64_le;

/// Hash table size (entries). Fixed so `idx & PNG_HASH_MASK` is provably in bounds.
const PNG_HASH_SIZE: usize = 1 << 16;
const PNG_HASH_MASK: usize = PNG_HASH_SIZE - 1;

/// DEFLATE window: farthest a match may reach back.
const WINDOW: usize = 32768;

/// Block length (input bytes). Fixed so every rung splits identically.
const PNG_BLOCK_LEN: usize = 128 * 1024;

/// Sequences per block: matches are at least 4 bytes and never cross a block
/// end, so this never fills (longest block is `PNG_BLOCK_LEN + MIN_BLOCK_LENGTH`).
pub(crate) const PNG_SEQ_STORE_LENGTH: usize = (PNG_BLOCK_LEN + MIN_BLOCK_LENGTH) / 4 + 2;

/// Tuning for the PNG parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PngParams {
    /// Look up 8-byte repeats in a hash table (otherwise runs only).
    pub hash: bool,
    /// Skip-ahead shift: after `g` bytes without a match the cursor
    /// advances by `1 + (g >> skip_shift)`.
    pub skip_shift: u32,
    /// Minimum hashed match length (4..=8). Only the low `min_match` bytes
    /// of the cursor are hashed and verified.
    pub min_match: u32,
    /// Hash-chain candidates to examine (1 = single-entry table, no chains).
    pub chain_depth: u32,
    /// Stop searching once a match is at least this long.
    pub nice_len: u32,
}

/// Longest skip-ahead step. Without a cap the step grows with the distance
/// since the last match (up to a whole block), so one long matchless stretch
/// could carry the parser across a compressible region it never looked at:
/// on a held-out manuscript scan, skip shift 5 lost 32% to shifts 4 and 6
/// that way. 256 costs 0-3% speed.
const SKIP_STEP_MAX: usize = 256;

/// Parser settings for [`CompressionLevel::png`](super::CompressionLevel::png)
/// efforts 2-9 (effort 1 is the ultra-fast encoder in `png_ultra`; 10 and up
/// use the lazy parsers with [`RunsGuard`]); `None` outside that range.
///
/// Chosen on 146-150 held-out imazen-26 cluster representatives at three
/// sizes for per-image monotonicity, not only aggregate position: the knobs
/// only move toward more work (skip shift and chain depth never decrease, one
/// min-match switch at effort 4, covered by `monotonicity_fallback`). See
/// `benchmarks/png_mode_2026-10-06.md`.
pub(crate) fn png_params(effort: u32) -> Option<PngParams> {
    // (hash, skip_shift, min_match, chain_depth, nice_len)
    let (hash, skip_shift, min_match, chain_depth, nice_len) = match effort {
        2 => (false, 4, 8, 1, 258),
        3 => (true, 4, 8, 1, 258),
        4 => (true, 4, 5, 1, 258),
        5 => (true, 6, 5, 1, 258),
        6 => (true, 6, 5, 2, 32),
        7 => (true, 6, 5, 4, 32),
        8 => (true, 6, 5, 8, 32),
        9 => (true, 6, 5, 16, 32),
        _ => return None,
    };
    Some(PngParams {
        hash,
        skip_shift,
        min_match,
        chain_depth,
        nice_len,
    })
}

/// Hash table of absolute positions (truncated to `u32`), optionally chained.
///
/// Stale or wrapped entries are harmless: every candidate is verified by
/// comparing `min_match` bytes before it is used, and chain walks stop as soon
/// as the distance stops increasing or leaves the window.
#[derive(Clone)]
pub(crate) struct PngMatchfinder {
    tab: Box<[u32; PNG_HASH_SIZE]>,
    /// Previous position with the same hash, indexed by `pos % WINDOW`.
    links: Option<Box<[u32; WINDOW]>>,
    /// Active table mask (small inputs only clear and use a prefix).
    mask: usize,
    /// Runs-only parse of the current block (see the guard in `compress_png`).
    runs: Box<RunsParse>,
}

#[derive(Clone)]
pub(crate) struct RunsParse {
    seqs: Vec<Sequence>,
    freqs: DeflateFreqs,
    /// Codes built from `freqs` by the last cost comparison.
    codes: DeflateCodes,
    n: usize,
}

impl PngMatchfinder {
    pub(crate) fn new(chained: bool) -> Self {
        Self {
            tab: alloc::vec![0u32; PNG_HASH_SIZE]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            links: chained.then(|| {
                alloc::vec![0u32; WINDOW]
                    .into_boxed_slice()
                    .try_into()
                    .ok()
                    .unwrap()
            }),
            mask: PNG_HASH_MASK,
            runs: Box::new(RunsParse {
                seqs: alloc::vec![Sequence::default(); PNG_SEQ_STORE_LENGTH],
                freqs: DeflateFreqs::default(),
                codes: DeflateCodes::default(),
                n: 0,
            }),
        }
    }

    /// Reset for an input of `len` bytes, sizing the active table to it.
    fn init(&mut self, len: usize) {
        // ~1 entry per 2 input bytes, between 1K and 64K entries.
        let want = (len / 2).next_power_of_two().clamp(1 << 10, PNG_HASH_SIZE);
        self.mask = want - 1;
        self.tab[..want].fill(0);
    }

    #[inline(always)]
    fn index(&self, key: u64) -> usize {
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize & self.mask & PNG_HASH_MASK
    }

    #[inline(always)]
    fn insert(&mut self, key: u64, pos: usize) {
        let i = self.index(key);
        let prev = self.tab[i];
        self.tab[i] = pos as u32;
        if let Some(links) = self.links.as_mut() {
            links[pos & (WINDOW - 1)] = prev;
        }
    }

    /// Insert `ip` and return the longest verified match (nearest on ties)
    /// as `(start, length, distance)`; length 0 = none.
    ///
    /// `key` is the cursor's 8 bytes shifted left by `mm_shift`, so only the
    /// low `min_match` bytes take part. Matches extend backwards down to
    /// `last` (the start of the pending literal run) and forwards to 258.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn find(
        &mut self,
        input: &[u8],
        ip: usize,
        last: usize,
        end: usize,
        key: u64,
        cfg: &ParseCfg,
    ) -> (usize, usize, usize) {
        let max_len = DEFLATE_MAX_MATCH_LEN as usize;
        let (mm, mm_shift) = (cfg.mm, cfg.mm_shift);
        let i = self.index(key);
        let mut cand = self.tab[i];
        self.tab[i] = ip as u32;
        if let Some(links) = self.links.as_mut() {
            links[ip & (WINDOW - 1)] = cand;
        }

        let mut best = (0, 0, 0);
        let mut prev_dist = 0;
        for _ in 0..cfg.depth {
            let dist = (ip as u32).wrapping_sub(cand) as usize;
            if dist <= prev_dist || dist > WINDOW || dist > ip {
                break;
            }
            prev_dist = dist;
            if load_u64_le(input, ip - dist) << mm_shift == key {
                // Forward first; the backward pass (bounded by the pending
                // literals) only runs if the total could beat the best so far.
                let cap = max_len.min(end - ip);
                let fwd = mm + extend_forward(input, ip + mm, ip + mm - dist, cap - mm);
                let max_back = (ip - last).min(ip - dist).min(max_len - fwd);
                if fwd + max_back <= best.1 {
                    // Later candidates are farther away: no better by length.
                } else {
                    let back = extend_backward(input, ip, ip - dist, max_back);
                    let (ms, len) = (ip - back, back + fwd);
                    // Candidates come nearest first, so ties keep the cheaper distance.
                    if len > best.1 {
                        best = (ms, len, dist);
                        if len >= cfg.nice {
                            break;
                        }
                    }
                }
            }
            match self.links.as_ref() {
                Some(links) => cand = links[(ip - dist) & (WINDOW - 1)],
                None => break,
            }
        }
        best
    }
}

/// Count literal frequencies for a finished block (4 interleaved histograms).
fn count_literals(block: &[u8], sequences: &[Sequence], freqs: &mut DeflateFreqs) {
    let mut h = [[0u32; 256]; 4];
    let mut pos = 0usize;
    for seq in sequences {
        let n = seq.litrunlen() as usize;
        let lits = &block[pos..pos + n];
        let (chunks, rest) = lits.as_chunks::<4>();
        for c in chunks {
            h[0][c[0] as usize] += 1;
            h[1][c[1] as usize] += 1;
            h[2][c[2] as usize] += 1;
            h[3][c[3] as usize] += 1;
        }
        for &b in rest {
            h[0][b as usize] += 1;
        }
        pos += n + seq.length() as usize;
    }
    for (i, f) in freqs.litlen[..256].iter_mut().enumerate() {
        *f += h[0][i] + h[1][i] + h[2][i] + h[3][i];
    }
}

/// Number of bytes equal to `b` in `data[start..cap]`, from the start.
#[inline(always)]
fn run_length(data: &[u8], start: usize, cap: usize, b: u8) -> usize {
    let pat = u64::from_ne_bytes([b; 8]);
    let mut n = 0;
    while start + n + 8 <= cap {
        let x = load_u64_le(data, start + n) ^ pat;
        if x != 0 {
            return n + (x.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while start + n < cap && data[start + n] == b {
        n += 1;
    }
    n
}

/// Number of equal bytes immediately before `data[ai]` and `data[bi]`
/// (`bi < ai`), capped at `max` (requires `max <= bi`).
#[inline(always)]
fn extend_backward(data: &[u8], ai: usize, bi: usize, max: usize) -> usize {
    let mut n = 0;
    while n + 8 <= max {
        let x = load_u64_le(data, ai - n - 8) ^ load_u64_le(data, bi - n - 8);
        if x != 0 {
            return n + (x.leading_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < max && data[ai - n - 1] == data[bi - n - 1] {
        n += 1;
    }
    n
}

/// Length of the common prefix of `a[ai..]` and `a[bi..]`, capped at `max`.
/// Requires `bi < ai` and `ai + max <= a.len()`.
#[inline(always)]
fn extend_forward(data: &[u8], ai: usize, bi: usize, max: usize) -> usize {
    let mut len = 0;
    while len + 8 <= max {
        let x = load_u64_le(data, ai + len) ^ load_u64_le(data, bi + len);
        if x != 0 {
            return len + (x.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while len < max && data[ai + len] == data[bi + len] {
        len += 1;
    }
    len
}

/// Per-call parser settings derived from [`PngParams`].
struct ParseCfg {
    skip_shift: u32,
    mm: usize,
    mm_shift: u32,
    depth: u32,
    nice: usize,
}

/// Parse `input[begin..end]` into `seqs`/`freqs` (literal frequencies
/// included, end-of-block not). Matches never cross `end`, so every rung
/// splits the input into identical blocks. Returns the index of the final
/// (literal-only) sequence.
///
/// `mf = None` parses runs only.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn parse_block(
    input: &[u8],
    begin: usize,
    end: usize,
    mut mf: Option<&mut PngMatchfinder>,
    cfg: &ParseCfg,
    seqs: &mut [Sequence],
    freqs: &mut DeflateFreqs,
    stop: &impl enough::Stop,
) -> Result<usize, CompressionError> {
    let max_len = DEFLATE_MAX_MATCH_LEN as usize;
    // 8-byte loads at the cursor must stay inside the block.
    let scan_end = end.saturating_sub(8);

    freqs.reset();
    seqs[0].litrunlen_and_length = 0;
    let mut seq_idx = 0usize;
    let mut ip = begin;
    let mut last = begin; // start of the pending literal run
    let mut next_stop_check = ip + STOP_CHECK_INTERVAL;

    while ip < scan_end {
        if ip >= next_stop_check {
            stop.check()?;
            next_stop_check = ip + STOP_CHECK_INTERVAL;
        }

        let v = load_u64_le(input, ip);

        // (match_start, length, distance); length 0 = no match.
        let (ms, len, dist) = if v as u32 == (v >> 8) as u32 {
            // Run: input[ip..ip + 5] are all `b`. Encode as a distance-1
            // match starting at ip + 1, then grow it both ways.
            let b = input[ip];
            let run_end = ip + 5;
            let min_start = last.max(1).max(run_end.saturating_sub(max_len));
            let mut ms = ip + 1;
            while ms > min_start && input[ms - 2] == b {
                ms -= 1;
            }
            let cap = (ms + max_len).min(end);
            let e = run_end + run_length(input, run_end, cap, b);
            (ms, e - ms, 1)
        } else if let Some(mf) = mf.as_deref_mut() {
            mf.find(input, ip, last, end, v << cfg.mm_shift, cfg)
        } else {
            (0, 0, 0)
        };

        if len == 0 {
            ip += (1 + ((ip - last) >> cfg.skip_shift)).min(SKIP_STEP_MAX);
            continue;
        }

        seqs[seq_idx].litrunlen_and_length += (ms - last) as u32;
        seq_idx = choose_match(freqs, len as u32, dist as u32, seqs, seq_idx);
        let m_end = ms + len;

        // Index the positions covered by a hashed match (runs are skipped:
        // their 8-byte windows are all identical).
        if dist > 1
            && let Some(mf) = mf.as_deref_mut()
        {
            let hi = m_end.min(input.len() - 8);
            for p in ip + 1..hi {
                mf.insert(load_u64_le(input, p) << cfg.mm_shift, p);
            }
        }

        ip = m_end;
        last = m_end;
    }

    seqs[seq_idx].litrunlen_and_length += (end - last) as u32;
    count_literals(&input[begin..end], &seqs[..=seq_idx], freqs);
    Ok(seq_idx)
}

/// The runs-only guard for `png()` rungs built on the general lazy parsers
/// (`png(10..)`): before a block is written, parse the same bytes runs-only
/// and report whether that parse is cheaper. Same rule as the guard in
/// `compress_png_inner`, including the pause after a clear loss.
#[derive(Clone)]
pub(crate) struct RunsGuard {
    parse: RunsParse,
    skip: u32,
}

impl RunsGuard {
    /// A guard for blocks of up to `max_block` input bytes.
    pub(crate) fn new(max_block: usize) -> Self {
        Self {
            parse: RunsParse {
                seqs: alloc::vec![Sequence::default(); (max_block + MIN_BLOCK_LENGTH) / 4 + 2],
                freqs: DeflateFreqs::default(),
                codes: DeflateCodes::default(),
                n: 0,
            },
            skip: 0,
        }
    }

    /// Reset between compressions.
    pub(crate) fn reset(&mut self) {
        self.skip = 0;
    }

    /// Parse `input[begin..end]` runs-only and compare it with the main parse
    /// (literal/match frequencies `freqs`, end-of-block not counted).
    /// `None`: the comparison was skipped (paused after a clear loss).
    /// `Some(true)`: runs-only is cheaper; [`parse`](Self::parse) holds it
    /// with its codes. `Some(false)`: the main parse wins and `main_codes`
    /// now hold its codes (built from `freqs` + end-of-block).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prefer_runs(
        &mut self,
        input: &[u8],
        begin: usize,
        end: usize,
        freqs: &DeflateFreqs,
        main_codes: &mut DeflateCodes,
        static_codes: &DeflateCodes,
        stop: &impl enough::Stop,
    ) -> Result<Option<bool>, CompressionError> {
        if self.skip > 0 {
            self.skip -= 1;
            return Ok(None);
        }
        let runs_bits = self.parse_runs(input, begin, end, static_codes, stop)?;
        let main_bits = block_bits(freqs, main_codes, static_codes);
        Ok(Some(self.judge(runs_bits, main_bits)))
    }

    /// For parsers that build their own codes (the near-optimal `png()`
    /// rungs): parse `input[begin..end]` runs-only and report whether it beats
    /// a main parse of `main_bits` (as [`encoded_bits`] counts them). `false`
    /// while paused after a clear loss; when `true`, [`parse`](Self::parse)
    /// holds the runs-only parse with its codes.
    pub(crate) fn runs_beat(
        &mut self,
        input: &[u8],
        begin: usize,
        end: usize,
        main_bits: u32,
        static_codes: &DeflateCodes,
        stop: &impl enough::Stop,
    ) -> Result<bool, CompressionError> {
        if self.skip > 0 {
            self.skip -= 1;
            return Ok(false);
        }
        let runs_bits = self.parse_runs(input, begin, end, static_codes, stop)?;
        Ok(self.judge(runs_bits, main_bits))
    }

    /// Runs-only parse of `input[begin..end]`; returns its size in bits.
    fn parse_runs(
        &mut self,
        input: &[u8],
        begin: usize,
        end: usize,
        static_codes: &DeflateCodes,
        stop: &impl enough::Stop,
    ) -> Result<u32, CompressionError> {
        // png(2)'s runs-only settings.
        let cfg = ParseCfg {
            skip_shift: 4,
            mm: 8,
            mm_shift: 0,
            depth: 1,
            nice: DEFLATE_MAX_MATCH_LEN as usize,
        };
        let r = &mut self.parse;
        r.n = parse_block(
            input,
            begin,
            end,
            None,
            &cfg,
            &mut r.seqs,
            &mut r.freqs,
            stop,
        )?;
        Ok(block_bits(&r.freqs, &mut r.codes, static_codes))
    }

    /// Whether runs-only (`runs_bits`) wins; pauses after a clear loss.
    fn judge(&mut self, runs_bits: u32, main_bits: u32) -> bool {
        if runs_bits > main_bits + main_bits / 8 {
            self.skip = 3;
        }
        runs_bits < main_bits
    }

    /// The last runs-only parse: its sequences, frequencies and codes.
    pub(crate) fn parse(&mut self) -> (&[Sequence], &mut DeflateFreqs, &DeflateCodes) {
        (
            &self.parse.seqs[..=self.parse.n],
            &mut self.parse.freqs,
            &self.parse.codes,
        )
    }
}

/// Size (bits) of the cheaper of the dynamic and static encodings of a block
/// whose frequencies (end-of-block included) and dynamic codes are given; the
/// same count [`RunsGuard`] uses for its runs-only parse.
pub(crate) fn encoded_bits(
    freqs: &DeflateFreqs,
    codes: &DeflateCodes,
    static_codes: &DeflateCodes,
) -> u32 {
    let dynamic = block_symbol_cost(freqs, &codes.lens_litlen, &codes.lens_offset)
        + dynamic_header_bits(codes);
    let fixed = block_symbol_cost(freqs, &static_codes.lens_litlen, &static_codes.lens_offset);
    dynamic.min(fixed)
}

/// Size (bits) of the cheaper of the dynamic and static Huffman encodings of
/// a block with these frequencies (end-of-block symbol not yet counted), as
/// `finish_block` would write it.
fn block_bits(freqs: &DeflateFreqs, codes: &mut DeflateCodes, static_codes: &DeflateCodes) -> u32 {
    let mut f = freqs.clone();
    f.litlen[DEFLATE_END_OF_BLOCK as usize] += 1;
    make_huffman_codes(&f, codes);
    let dynamic =
        block_symbol_cost(&f, &codes.lens_litlen, &codes.lens_offset) + dynamic_header_bits(codes);
    let fixed = block_symbol_cost(&f, &static_codes.lens_litlen, &static_codes.lens_offset);
    dynamic.min(fixed)
}

impl Compressor {
    /// PNG-tuned greedy parser (see module docs).
    pub(super) fn compress_png(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        params: PngParams,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut mf = if params.hash {
            Some(self.png_mf.take().expect("png_mf is present between calls"))
        } else {
            None
        };
        let result = self.compress_png_inner(mf.as_deref_mut(), os, input, params, stop);
        // Restore on every path: an early return (stop) must leave the
        // compressor reusable.
        if mf.is_some() {
            self.png_mf = mf;
        }
        result
    }

    fn compress_png_inner(
        &mut self,
        mut mf: Option<&mut PngMatchfinder>,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        params: PngParams,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        if let Some(mf) = mf.as_deref_mut() {
            mf.init(input.len() - self.chunk_start);
        }

        let in_end = input.len();
        let mut ip = self.chunk_start;
        let mm = params.min_match.clamp(4, 8) as usize;
        let cfg = ParseCfg {
            skip_shift: params.skip_shift,
            mm,
            mm_shift: 64 - 8 * mm as u32,
            depth: params.chain_depth.max(1),
            nice: params.nice_len.clamp(mm as u32, DEFLATE_MAX_MATCH_LEN) as usize,
        };

        // Dictionary warm-up (parallel chunks): index the preceding window.
        if let Some(mf) = mf.as_mut() {
            let warm_end = ip.min(in_end.saturating_sub(8));
            for p in ip.saturating_sub(WINDOW)..warm_end {
                mf.insert(load_u64_le(input, p) << cfg.mm_shift, p);
            }
        }

        // Blocks left before the runs-only comparison runs again.
        let mut runs_skip = 0u32;

        while ip < in_end && !os.overflow {
            stop.check()?;
            let begin = ip;
            let end = if in_end - begin < PNG_BLOCK_LEN + MIN_BLOCK_LENGTH {
                in_end
            } else {
                begin + PNG_BLOCK_LEN
            };

            let n = parse_block(
                input,
                begin,
                end,
                mf.as_deref_mut(),
                &cfg,
                &mut self.sequences,
                &mut self.freqs,
                stop,
            )?;

            // Guard: on flat-colour art, far LZ77 matches split runs and
            // spread the offset codes (which also makes every run's
            // distance-1 code longer), and the runs-only parse of a block can
            // be up to ~10% smaller. Parse the block runs-only too and emit
            // the cheaper parse. When runs-only loses by more than 1/8
            // (photo-like content) the next 3 blocks skip the comparison.
            let mut use_runs = false;
            let mut compared = false;
            if let Some(mf) = mf.as_deref_mut() {
                if runs_skip > 0 {
                    runs_skip -= 1;
                } else {
                    compared = true;
                    let runs = &mut mf.runs;
                    runs.n = parse_block(
                        input,
                        begin,
                        end,
                        None,
                        &cfg,
                        &mut runs.seqs,
                        &mut runs.freqs,
                        stop,
                    )?;
                    let main_bits = block_bits(&self.freqs, &mut self.codes, &self.static_codes);
                    let runs_bits = block_bits(&runs.freqs, &mut runs.codes, &self.static_codes);
                    use_runs = runs_bits < main_bits;
                    if runs_bits > main_bits + main_bits / 8 {
                        runs_skip = 3;
                    }
                }
            }

            let block = &input[begin..end];
            let is_final = !self.force_nonfinal && end >= in_end;
            match mf.as_deref_mut() {
                // Codes were built by the cost comparison; don't build them again.
                Some(mf) if use_runs => finish_block_with_codes(
                    os,
                    block,
                    block.len(),
                    &mf.runs.seqs[..=mf.runs.n],
                    &mut mf.runs.freqs,
                    &mf.runs.codes,
                    &self.static_codes,
                    is_final,
                ),
                _ if compared => finish_block_with_codes(
                    os,
                    block,
                    block.len(),
                    &self.sequences[..=n],
                    &mut self.freqs,
                    &self.codes,
                    &self.static_codes,
                    is_final,
                ),
                _ => finish_block(
                    os,
                    block,
                    block.len(),
                    &self.sequences[..=n],
                    &mut self.freqs,
                    &mut self.codes,
                    &self.static_codes,
                    is_final,
                ),
            }

            ip = end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CompressionLevel, Compressor};
    use alloc::vec;
    use alloc::vec::Vec;

    /// Deterministic xorshift bytes.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Synthetic filtered RGB image: filter byte + small residuals, flat
    /// regions, and repeated rows.
    fn filtered_image(w: usize, h: usize, seed: u64) -> Vec<u8> {
        let n = noise(w * h * 3, seed);
        let mut out = Vec::new();
        for y in 0..h {
            out.push((y % 5) as u8);
            for x in 0..w * 3 {
                let r = n[y * w * 3 + x];
                let v = match (y / 8) % 4 {
                    0 => 0,                          // flat
                    1 => (r % 5).wrapping_sub(2),    // small residuals
                    2 => n[(y % 8) * w * 3 + x] % 7, // repeats every 8 rows
                    _ => r,                          // noise
                };
                out.push(v);
            }
        }
        out
    }

    fn roundtrip(level: CompressionLevel, data: &[u8]) -> usize {
        let mut c = Compressor::new(level);
        let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
        let n = c
            .zlib_compress(data, &mut out, enough::Unstoppable)
            .unwrap_or_else(|e| panic!("{level:?} len {}: {e:?}", data.len()));
        let out = &out[..n];

        let mut back = vec![0u8; data.len()];
        let got = crate::Decompressor::new()
            .zlib_decompress(out, &mut back, enough::Unstoppable)
            .unwrap_or_else(|e| panic!("{level:?} len {}: {e:?}", data.len()));
        assert_eq!(got.output_written, data.len());
        assert!(
            back == data,
            "{level:?} len {}: zenflate mismatch",
            data.len()
        );

        let mz = miniz_oxide::inflate::decompress_to_vec_zlib(out)
            .unwrap_or_else(|e| panic!("{level:?} len {}: miniz {e:?}", data.len()));
        assert!(mz == data, "{level:?} len {}: miniz mismatch", data.len());
        n
    }

    fn inputs() -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = vec![Vec::new()];
        for n in [1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 63, 64, 65, 300] {
            v.push(noise(n, n as u64));
            v.push(vec![7u8; n]);
        }
        v.push(vec![0u8; 100_000]); // runs far past 258
        v.push(noise(200_000, 3)); // incompressible: exercises skip-ahead + stored
        let mut w = noise(32768 + 64, 9);
        w.extend_from_within(..300); // repeat at exactly the window distance
        w.extend_from_within(1..301); // and one byte past it
        v.push(w);
        v.push(filtered_image(97, 61, 1));
        v.push(filtered_image(1000, 400, 2)); // > one 128 KiB block
        let mut mixed = Vec::new();
        for i in 0..3000u32 {
            mixed.extend_from_slice(&i.to_le_bytes());
            mixed.extend_from_slice(&[0, 0, 0, 0, 0, 0, (i % 3) as u8]);
        }
        v.push(mixed);
        v
    }

    #[test]
    fn png_roundtrip_all_efforts() {
        let inputs = inputs();
        for effort in 0..=16 {
            let level = CompressionLevel::png(effort);
            for data in &inputs {
                roundtrip(level, data);
            }
        }
    }

    /// Randomized structure (runs, near and far repeats, noise) at random
    /// lengths: a deterministic stand-in for the fuzz target.
    #[test]
    fn png_roundtrip_randomized() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..300 {
            let target = (next() % 70_000) as usize;
            let mut data = Vec::with_capacity(target);
            while data.len() < target {
                let n = 1 + (next() % 600) as usize;
                match next() % 4 {
                    0 => data.extend(core::iter::repeat_n(next() as u8, n)),
                    1 if !data.is_empty() => {
                        let dist = 1 + (next() as usize % data.len().min(40_000));
                        for _ in 0..n {
                            data.push(data[data.len() - dist]);
                        }
                    }
                    2 => data.extend((0..n).map(|_| (next() % 3) as u8)),
                    _ => data.extend((0..n).map(|_| next() as u8)),
                }
            }
            let effort = 1 + (next() % 14) as u32;
            roundtrip(CompressionLevel::png(effort), &data);
        }
    }

    /// One compressor reused across inputs of different sizes (hash table
    /// prefix sizing + stale entries/links from earlier calls).
    #[test]
    fn png_compressor_reuse() {
        let inputs = inputs();
        for effort in [1, 3, 6, 10, 12] {
            let level = CompressionLevel::png(effort);
            let mut c = Compressor::new(level);
            for data in inputs.iter().chain(inputs.iter().rev()) {
                let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
                let n = c
                    .zlib_compress(data, &mut out, enough::Unstoppable)
                    .unwrap();
                let fresh = roundtrip(level, data);
                assert_eq!(n, fresh, "png({effort}) reuse changed output size");
            }
        }
    }

    /// Along the ladder, output (following `monotonicity_fallback` like a
    /// caller wanting monotonic output) may grow only by within-strategy
    /// butterfly noise, never by the multi-percent steps a strategy switch
    /// can cause.
    #[test]
    fn png_ladder_monotone_on_filtered_data() {
        for seed in [5, 6, 7] {
            let img = filtered_image(500, 200, seed);
            let mut prev = usize::MAX;
            for effort in 1..=12 {
                let mut level = CompressionLevel::png(effort);
                let mut n = roundtrip(level, &img);
                while let Some(fb) = level.monotonicity_fallback() {
                    n = n.min(roundtrip(fb, &img));
                    level = fb;
                }
                assert!(
                    n <= prev.saturating_add(prev / 1000),
                    "seed {seed}: png({effort}) {n} > previous rung {prev}"
                );
                prev = prev.min(n);
            }
            let runs = roundtrip(CompressionLevel::png(2), &img);
            assert!(
                prev < runs,
                "seed {seed}: chains {prev} not below runs-only {runs}"
            );
        }
    }

    #[test]
    fn png_level_mapping() {
        assert_eq!(CompressionLevel::png(0).effort(), 0);
        assert_eq!(CompressionLevel::png(500).effort(), 200);
        // 19-30 are near-optimal with png() block ends and the runs-only
        // guard; 31+ (full optimal) equal new(31+).
        let img = filtered_image(200, 100, 7);
        for (effort, general) in [(31, 31), (40, 40)] {
            let a = CompressionLevel::png(effort);
            let b = CompressionLevel::new(general);
            assert_eq!(
                roundtrip(a, &img),
                roundtrip(b, &img),
                "png({effort}) vs new({general})"
            );
        }
        assert_eq!(CompressionLevel::png(20).effort(), 20);

        let chain = |mut l: CompressionLevel| {
            let mut v = vec![l.effort()];
            while let Some(fb) = l.monotonicity_fallback() {
                v.push(fb.effort());
                l = fb;
            }
            v
        };
        assert_eq!(chain(CompressionLevel::png(1)), [1]);
        assert_eq!(chain(CompressionLevel::png(2)), [2, 1]);
        assert_eq!(chain(CompressionLevel::png(3)), [3]);
        assert_eq!(chain(CompressionLevel::png(10)), [10, 9, 3]);
        assert_eq!(chain(CompressionLevel::png(16)), [16, 9, 3]);
        assert_eq!(chain(CompressionLevel::png(20)), [20, 18, 9, 3]);
        assert_eq!(chain(CompressionLevel::png(40)), [40, 30, 18, 9, 3]);
    }

    /// The runs-only guard only ever swaps in a cheaper block: on flat-colour
    /// art the guarded lazy and near-optimal rungs beat the same parsers
    /// without it.
    #[test]
    fn png_lazy_rungs_guarded() {
        // Flat colour bands with sparse noise: far matches split the runs.
        let mut img = Vec::new();
        let mut x = 0x2545_F491u32;
        for y in 0..300 {
            img.push(1);
            for i in 0..600 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                img.push(if x.is_multiple_of(97) {
                    x as u8
                } else {
                    ((i / 40 + y / 25) % 4) as u8 * 40
                });
            }
        }
        // Near-optimal rungs: wider bands (period 51 bytes), noise 1 in 61.
        let mut bands = Vec::new();
        for y in 0..300 {
            bands.push(0);
            for i in 0..900 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                bands.push(if x.is_multiple_of(61) {
                    x as u8
                } else {
                    ((i / 51 + y / 9) % 3) as u8 * 85
                });
            }
        }
        let sized = |img: &[u8], effort: u32, guard: bool| {
            let mut c = Compressor::new(CompressionLevel::png(effort));
            if !guard {
                c.runs_guard = None;
            }
            let mut out = vec![0u8; Compressor::zlib_compress_bound(img.len())];
            c.zlib_compress(img, &mut out, enough::Unstoppable).unwrap()
        };
        for (img, efforts) in [
            (&img, &[10, 12, 15, 18][..]),
            (&bands, &[19, 20, 22, 26][..]),
        ] {
            let mut smaller = 0;
            for &effort in efforts {
                let (with, without) = (sized(img, effort, true), sized(img, effort, false));
                assert!(
                    with <= without,
                    "png({effort}): {with} vs {without} unguarded"
                );
                smaller += usize::from(with < without);
            }
            assert!(
                smaller > 0,
                "the guard never picked the runs-only parse in {efforts:?}"
            );
        }
    }

    /// png(10..=18) take block ends from the input alone
    /// (`input_block_end`): they are deterministic, strictly increasing and
    /// end at the input's end, and rungs built on them round-trip.
    #[test]
    fn png_lazy_rungs_share_block_ends() {
        let img = filtered_image(900, 300, 3);
        let ends = |data: &[u8]| {
            let mut v = vec![];
            let mut b = 0;
            while b < data.len() {
                b = crate::compress::block_split::input_block_end(data, b, data.len(), 300_000);
                v.push(b);
            }
            v
        };
        let e = ends(&img);
        assert!(e.len() > 1, "test image should split");
        assert_eq!(e, ends(&img));
        assert_eq!(*e.last().unwrap(), img.len());
        for w in e.windows(2) {
            assert!(w[1] > w[0]);
        }
        for effort in [10, 14, 18] {
            let mut out = vec![0u8; Compressor::zlib_compress_bound(img.len())];
            let n = Compressor::new(CompressionLevel::png(effort))
                .zlib_compress(&img, &mut out, enough::Unstoppable)
                .unwrap();
            let back = miniz_oxide::inflate::decompress_to_vec_zlib(&out[..n]).unwrap();
            assert!(back == img, "png({effort}) roundtrip");
        }
    }

    /// png(19..=30) (near-optimal) end their blocks where
    /// `input_block_end` says, like the lazy rungs, and never lose to their
    /// own parse by more than the runs-only guard allows: each round-trips.
    #[test]
    fn png_near_optimal_rungs_share_block_ends() {
        let img = filtered_image(900, 300, 3);
        let mut want = vec![];
        let mut b = 0;
        while b < img.len() {
            b = crate::compress::block_split::input_block_end(
                &img,
                b,
                img.len(),
                crate::compress::SOFT_MAX_BLOCK_LENGTH,
            );
            want.push(b);
        }
        assert!(want.len() > 1, "test image should split");
        for effort in [19, 20, 21, 22, 23, 26, 30] {
            let mut c = Compressor::new(CompressionLevel::png(effort));
            let mut out = vec![0u8; Compressor::zlib_compress_bound(img.len())];
            let n = c
                .zlib_compress(&img, &mut out, enough::Unstoppable)
                .unwrap();
            assert_eq!(c.test_block_ends, want, "png({effort}) block ends");
            let back = miniz_oxide::inflate::decompress_to_vec_zlib(&out[..n]).unwrap();
            assert!(back == img, "png({effort}) roundtrip");
        }
        // new() keeps libdeflate's parse-driven splitting.
        let mut c = Compressor::new(CompressionLevel::new(23));
        let mut out = vec![0u8; Compressor::zlib_compress_bound(img.len())];
        c.zlib_compress(&img, &mut out, enough::Unstoppable)
            .unwrap();
        assert_ne!(c.test_block_ends, want);
    }

    #[cfg(feature = "threads")]
    #[test]
    fn png_parallel_gzip() {
        let img = filtered_image(1500, 700, 11);
        for effort in [1, 4, 9, 14] {
            let mut c = Compressor::new(CompressionLevel::png(effort));
            let mut out = vec![0u8; Compressor::gzip_compress_bound(img.len()) + 4096];
            let n = c
                .gzip_compress_parallel(&img, &mut out, 4, enough::Unstoppable)
                .unwrap();
            let mut back = vec![0u8; img.len()];
            let got = crate::Decompressor::new()
                .gzip_decompress(&out[..n], &mut back, enough::Unstoppable)
                .unwrap();
            assert_eq!(got.output_written, img.len());
            assert!(back == img, "png({effort}) parallel gzip mismatch");
        }
    }
}

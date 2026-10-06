//! Ultra-fast PNG compression: literals and zero runs, no match search.
//!
//! PNG filters turn flat regions into runs of zero bytes and everything else
//! into small residuals. This encoder emits exactly two token kinds:
//!
//! - **Zero runs**, found by comparing 8-byte chunks against zero, coded as a
//!   literal 0 followed by distance-1 matches.
//! - **Literals** for everything else, eight per loop iteration with no
//!   per-byte branching.
//!
//! One Huffman table serves the whole stream, built from token counts: of
//! the whole input up to 64 KiB, otherwise of evenly spaced segments covering
//! about 1/16 of it. Code lengths are capped at 12 bits so four literals always
//! fit one 48-bit write; from 512 KiB of input a 64K-entry table codes two
//! literals per lookup. Blocks that would expand are rewritten as stored
//! blocks.
//!
//! The token model comes from fdeflate's ultra-fast mode
//! ([fdeflate](https://github.com/image-rs/fdeflate), MIT OR Apache-2.0, the
//! image-rs developers).

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::Compressor;
use super::bitstream::OutputBitstream;
use super::block::{DeflateCodes, LENGTH_SLOT, write_dynamic_header_body};
use super::huffman::{canonical_codewords, make_huffman_code};
use crate::constants::*;
use crate::error::CompressionError;
use crate::fast_bytes::store_u64_le;

/// Input bytes per DEFLATE block (the unit of the stored-block fallback).
const ULTRA_BLOCK_LEN: usize = 256 * 1024;

/// Longest literal/length codeword: 4 literals fit 48 bits, so a 4-literal
/// write plus at most 7 pending bits never overflows the 64-bit buffer.
const MAX_LITLEN_CODEWORD_LEN: u32 = 12;

/// Inputs up to this size are counted whole; longer ones are sampled.
/// Counting costs a second pass, which on small inputs is a few
/// microseconds: on held-out 64x64 and 128x128 images the counted table is
/// 12-15% smaller than fdeflate's fixed PNG table (1.7-2.5x the time), and
/// the header shrinks because absent literals need no code.
const WHOLE_COUNT_MAX: usize = 64 * 1024;

/// Inputs at least this long use the 64K-entry pair table (~30 us to build,
/// +20% throughput once built).
const PAIR_LUT_MIN: usize = 512 * 1024;

/// Code tables for one stream.
#[derive(Clone)]
pub(crate) struct UltraState {
    codes: DeflateCodes,
    /// Literal `b`: codeword | length << 24.
    lit: [u32; 256],
    /// Literal pair `a | b << 8` (a first): codeword | length << 27.
    pair: Box<[u32; 65536]>,
    /// Distance-1 match of length 3..=258: length code, extra bits and the
    /// distance code, as codeword | length << 24.
    run: [u32; DEFLATE_MAX_MATCH_LEN as usize + 1],
    /// `k` zero literals back to back (k = 0..=16).
    zeros: [u64; 17],
    zeros_len: [u8; 17],
    /// Runs of at most this many zeros are cheaper as zero literals.
    lit_run_max: usize,
    /// Dynamic header (after BFINAL/BTYPE) for `codes`.
    header: Vec<u8>,
    header_bits: usize,
}

impl UltraState {
    pub(crate) fn new() -> Self {
        Self {
            codes: DeflateCodes::default(),
            lit: [0; 256],
            pair: alloc::vec![0u32; 65536]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            run: [0; DEFLATE_MAX_MATCH_LEN as usize + 1],
            zeros: [0; 17],
            zeros_len: [0; 17],
            lit_run_max: 0,
            header: Vec::new(),
            header_bits: 0,
        }
    }

    /// Build all lookup tables from `self.codes`.
    fn prepare(&mut self, pair_lut: bool) {
        let c = &self.codes;
        for b in 0..256 {
            // Literals absent from a fully counted input have no code.
            debug_assert!(c.lens_litlen[b] as u32 <= MAX_LITLEN_CODEWORD_LEN);
            self.lit[b] = c.codewords_litlen[b] | (c.lens_litlen[b] as u32) << 24;
        }
        if pair_lut {
            for hi in 0..256 {
                let (hc, hl) = (c.codewords_litlen[hi], c.lens_litlen[hi] as u32);
                let row = &mut self.pair[hi * 256..hi * 256 + 256];
                for (lo, e) in row.iter_mut().enumerate() {
                    let (lc, ll) = (c.codewords_litlen[lo], c.lens_litlen[lo] as u32);
                    *e = (lc | hc << ll) | (ll + hl) << 27;
                }
            }
        }
        let (dc, dl) = (c.codewords_offset[0], c.lens_offset[0] as u32);
        for len in DEFLATE_MIN_MATCH_LEN..=DEFLATE_MAX_MATCH_LEN {
            let slot = LENGTH_SLOT[len as usize] as usize;
            let sym = DEFLATE_FIRST_LEN_SYM as usize + slot;
            let (sc, sl) = (c.codewords_litlen[sym], c.lens_litlen[sym] as u32);
            let extra = DEFLATE_LENGTH_EXTRA_BITS[slot] as u32;
            let code = sc | (len - DEFLATE_LENGTH_BASE[slot] as u32) << sl | dc << (sl + extra);
            self.run[len as usize] = code | (sl + extra + dl) << 24;
        }
        let (zc, zl) = (c.codewords_litlen[0] as u64, c.lens_litlen[0] as u32);
        let mut acc = 0u64;
        for k in 0..17 {
            self.zeros[k] = acc;
            self.zeros_len[k] = (k as u32 * zl).min(255) as u8;
            if (k as u32 + 1) * zl <= 48 {
                acc |= zc << (k as u32 * zl);
            }
        }
        // Largest k whose zero literals are no dearer than literal + match.
        let run_bits = |k: usize| zl + (self.run[k - 1] >> 24);
        let mut max = 3;
        for k in 4..=16 {
            if k as u32 * zl > 48 || run_bits(k) < k as u32 * zl {
                break;
            }
            max = k;
        }
        self.lit_run_max = max;

        let mut hdr = alloc::vec![0u8; 1024];
        let mut os = OutputBitstream::new(&mut hdr);
        write_dynamic_header_body(&mut os, c);
        let bits = os.pos * 8 + os.bitcount as usize;
        if os.bitcount > 0 {
            os.buf[os.pos] = os.bitbuf as u8;
        }
        self.header_bits = bits;
        hdr.truncate(bits.div_ceil(8));
        self.header = hdr;
    }

    /// Build the code from token counts: of the whole input up to
    /// [`WHOLE_COUNT_MAX`], otherwise of a sample.
    fn set_table(&mut self, data: &[u8]) {
        let mut count = CountSink {
            freqs: [0; DEFLATE_NUM_LITLEN_SYMS as usize],
        };
        let whole = data.len() <= WHOLE_COUNT_MAX;
        if whole {
            tokenize(data, &mut count);
        } else {
            // Evenly spaced 4 KiB segments covering ~1/16 of the input. Short
            // segments cut long zero runs (512-byte segments: +0.8% on held-out
            // native images).
            const SEG: usize = 4096;
            let target = (data.len() / 16).clamp(32 * 1024, 128 * 1024);
            let nseg = target / SEG;
            let stride = (data.len() - SEG) / (nseg - 1);
            for i in 0..nseg {
                tokenize(&data[i * stride..i * stride + SEG], &mut count);
            }
        }
        let mut freqs = count.freqs;
        if whole {
            // Nonzero bytes are always literals, so their counts are exact and
            // absent ones need no code. How zero runs split into literal 0s and
            // lengths depends on block boundaries and the table's run
            // threshold: those symbols and end-of-block keep a prior.
            freqs[0] += 1;
            freqs[DEFLATE_END_OF_BLOCK as usize] += 1;
            for f in &mut freqs[DEFLATE_FIRST_LEN_SYM as usize..286] {
                *f += 1;
            }
        } else {
            // A sample can miss any symbol: every one gets a code.
            for f in &mut freqs[..286] {
                *f += 1;
            }
        }
        let c = &mut self.codes;
        make_huffman_code(
            DEFLATE_NUM_LITLEN_SYMS as usize,
            MAX_LITLEN_CODEWORD_LEN,
            &freqs,
            &mut c.lens_litlen,
            &mut c.codewords_litlen,
        );
        set_distance_code(c);
    }
}

/// Only distance 1 is used: two 1-bit codes (slot 0 and an unused slot 1).
fn set_distance_code(c: &mut DeflateCodes) {
    c.lens_offset = [0; DEFLATE_NUM_OFFSET_SYMS as usize];
    c.lens_offset[0] = 1;
    c.lens_offset[1] = 1;
    canonical_codewords(
        DEFLATE_NUM_OFFSET_SYMS as usize,
        &c.lens_offset,
        &mut c.codewords_offset,
        1,
    );
}

/// Receiver of the token stream.
trait Sink {
    fn lit(&mut self, b: u8);
    fn lits8(&mut self, v: u64);
    /// `n >= 1` zero bytes.
    fn zeros(&mut self, n: usize);
}

/// Split the bytes after a run's leading literal 0 into match lengths.
#[inline(always)]
fn for_each_match(mut m: usize, mut f: impl FnMut(usize)) {
    while m >= 261 {
        f(258);
        m -= 258;
    }
    if m > 258 {
        f(m - 3);
        m = 3;
    }
    f(m); // m < 3 means that many zero literals
}

/// Tokenize `data` into literals and zero runs.
#[inline(always)]
fn tokenize<S: Sink>(data: &[u8], sink: &mut S) {
    let (chunks, tail) = data.as_chunks::<8>();
    let mut run = 0usize;
    for c in chunks {
        let v = u64::from_le_bytes(*c);
        if v == 0 {
            run += 8;
            continue;
        }
        let mut lo = 0;
        if run > 0 {
            lo = (v.trailing_zeros() / 8) as usize; // zeros continuing the run
            sink.zeros(run + lo);
            run = 0;
        }
        let hi = (v.leading_zeros() / 8) as usize; // zeros that may start a run
        if lo == 0 && hi < 4 {
            sink.lits8(v);
            continue;
        }
        let end = if hi >= 4 { 8 - hi } else { 8 };
        for &b in &c[lo..end] {
            sink.lit(b);
        }
        if hi >= 4 {
            run = hi;
        }
    }
    for &b in tail {
        if b == 0 {
            run += 1;
        } else {
            if run > 0 {
                sink.zeros(run);
                run = 0;
            }
            sink.lit(b);
        }
    }
    if run > 0 {
        sink.zeros(run);
    }
}

/// Counts symbol frequencies (for the sampled table).
struct CountSink {
    freqs: [u32; DEFLATE_NUM_LITLEN_SYMS as usize],
}

impl Sink for CountSink {
    #[inline(always)]
    fn lit(&mut self, b: u8) {
        self.freqs[b as usize] += 1;
    }
    #[inline(always)]
    fn lits8(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.freqs[b as usize] += 1;
        }
    }
    #[inline(always)]
    fn zeros(&mut self, n: usize) {
        if n < 6 {
            self.freqs[0] += n as u32;
            return;
        }
        self.freqs[0] += 1;
        for_each_match(n - 1, |m| {
            if m >= 3 {
                self.freqs[DEFLATE_FIRST_LEN_SYM as usize + LENGTH_SLOT[m] as usize] += 1;
            } else {
                self.freqs[0] += m as u32;
            }
        });
    }
}

/// Writes tokens to the output.
struct EmitSink<'a, 'b, const PAIR: bool> {
    buf: &'a mut [u8],
    pos: usize,
    bitbuf: u64,
    bitcount: u32,
    overflow: bool,
    st: &'b UltraState,
}

impl<const PAIR: bool> EmitSink<'_, '_, PAIR> {
    /// Add `n` bits; `bitcount + n <= 63` (callers stay below 56 after a flush).
    #[inline(always)]
    fn put(&mut self, bits: u64, n: u32) {
        self.bitbuf |= bits << self.bitcount;
        self.bitcount += n;
    }

    #[inline(always)]
    fn flush(&mut self) {
        if self.pos + 8 <= self.buf.len() {
            store_u64_le(self.buf, self.pos, self.bitbuf);
            self.pos += (self.bitcount >> 3) as usize;
            self.bitbuf >>= self.bitcount & !7;
            self.bitcount &= 7;
        } else {
            while self.bitcount >= 8 {
                if self.pos < self.buf.len() {
                    self.buf[self.pos] = self.bitbuf as u8;
                    self.pos += 1;
                    self.bitbuf >>= 8;
                    self.bitcount -= 8;
                } else {
                    self.overflow = true;
                    self.bitcount = 0;
                    self.bitbuf = 0;
                    return;
                }
            }
        }
    }

    /// Codes for four literals (bytes of `v`, low first): at most 48 bits.
    #[inline(always)]
    fn code4(&self, v: u32) -> (u64, u32) {
        if PAIR {
            let p0 = self.st.pair[(v & 0xFFFF) as usize];
            let p1 = self.st.pair[(v >> 16) as usize];
            let l0 = p0 >> 27;
            let bits = (p0 & 0x07FF_FFFF) as u64 | ((p1 & 0x07FF_FFFF) as u64) << l0;
            (bits, l0 + (p1 >> 27))
        } else {
            let lut = &self.st.lit;
            let e0 = lut[(v & 0xFF) as usize];
            let e1 = lut[((v >> 8) & 0xFF) as usize];
            let e2 = lut[((v >> 16) & 0xFF) as usize];
            let e3 = lut[(v >> 24) as usize];
            let (l0, l1, l2) = (e0 >> 24, e1 >> 24, e2 >> 24);
            let bits = (e0 & 0xFF_FFFF) as u64
                | ((e1 & 0xFF_FFFF) as u64) << l0
                | ((e2 & 0xFF_FFFF) as u64) << (l0 + l1)
                | ((e3 & 0xFF_FFFF) as u64) << (l0 + l1 + l2);
            (bits, l0 + l1 + l2 + (e3 >> 24))
        }
    }
}

impl<const PAIR: bool> Sink for EmitSink<'_, '_, PAIR> {
    #[inline(always)]
    fn lit(&mut self, b: u8) {
        let e = self.st.lit[b as usize];
        debug_assert!(e >> 24 > 0, "literal {b} has no code");
        self.put((e & 0xFF_FFFF) as u64, e >> 24);
        self.flush();
    }

    #[inline(always)]
    fn lits8(&mut self, v: u64) {
        debug_assert!(
            v.to_le_bytes()
                .iter()
                .all(|&b| self.st.lit[b as usize] >> 24 > 0),
            "literal without a code in {v:#x}"
        );
        let (h1, l1) = self.code4(v as u32);
        let (h2, l2) = self.code4((v >> 32) as u32);
        self.put(h1, l1);
        self.flush();
        self.put(h2, l2);
        self.flush();
    }

    #[inline(always)]
    fn zeros(&mut self, n: usize) {
        let st = self.st;
        if n <= st.lit_run_max {
            self.put(st.zeros[n], st.zeros_len[n] as u32);
            self.flush();
            return;
        }
        let z = st.lit[0];
        self.put((z & 0xFF_FFFF) as u64, z >> 24);
        for_each_match(n - 1, |m| {
            if m >= 3 {
                let e = st.run[m];
                self.put((e & 0xFF_FFFF) as u64, e >> 24);
            } else {
                self.put(st.zeros[m], st.zeros_len[m] as u32);
            }
            self.flush();
        });
    }
}

impl Compressor {
    /// Ultra-fast PNG encoder (see module docs).
    pub(super) fn compress_png_ultra(
        &mut self,
        os: &mut OutputBitstream<'_>,
        input: &[u8],
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let data = &input[self.chunk_start..];
        let mut st = self.png_ultra.take().unwrap();
        let pair = data.len() >= PAIR_LUT_MIN;
        st.set_table(data);
        st.prepare(pair);

        let result = if pair {
            self.ultra_blocks::<true>(os, data, &st, stop)
        } else {
            self.ultra_blocks::<false>(os, data, &st, stop)
        };
        self.png_ultra = Some(st);
        result
    }

    fn ultra_blocks<const PAIR: bool>(
        &self,
        os: &mut OutputBitstream<'_>,
        data: &[u8],
        st: &UltraState,
        stop: &impl enough::Stop,
    ) -> Result<(), CompressionError> {
        let mut begin = 0;
        while begin < data.len() && !os.overflow {
            stop.check()?;
            let end = if data.len() - begin < ULTRA_BLOCK_LEN + ULTRA_BLOCK_LEN / 4 {
                data.len()
            } else {
                begin + ULTRA_BLOCK_LEN
            };
            let block = &data[begin..end];
            let is_final = !self.force_nonfinal && end == data.len();

            let (pos0, bitbuf0, bitcount0) = (os.pos, os.bitbuf, os.bitcount);
            os.add_bits(is_final as u32, 1);
            os.add_bits(DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN, 2);
            os.flush_bits();
            // Cached header bits.
            let mut left = st.header_bits;
            for b in &st.header {
                let n = left.min(8) as u32;
                os.add_bits(*b as u32 & ((1 << n) - 1), n);
                os.flush_bits();
                left -= n as usize;
            }

            let mut sink = EmitSink::<PAIR> {
                buf: &mut *os.buf,
                pos: os.pos,
                bitbuf: os.bitbuf,
                bitcount: os.bitcount,
                overflow: os.overflow,
                st,
            };
            tokenize(block, &mut sink);
            let eob = st.codes.codewords_litlen[DEFLATE_END_OF_BLOCK as usize];
            let eob_len = st.codes.lens_litlen[DEFLATE_END_OF_BLOCK as usize] as u32;
            sink.put(eob as u64, eob_len);
            sink.flush();
            let (pos, bitbuf, bitcount, overflow) =
                (sink.pos, sink.bitbuf, sink.bitcount, sink.overflow);
            os.pos = pos;
            os.bitbuf = bitbuf;
            os.bitcount = bitcount;
            os.overflow = overflow;

            // Stored fallback: never end past where the stored form would
            // (write_uncompressed: 5 bytes per 64 KiB chunk, plus one when the
            // pending bits and the 3 header bits spill into a second byte).
            // Exact in bits, so the output always fits the compress bound.
            let stored_bytes =
                block.len() + 5 * block.len().div_ceil(0xFFFF) + (bitcount0 > 5) as usize;
            let block_bits = (os.pos - pos0) * 8 + os.bitcount as usize;
            if os.overflow || block_bits > stored_bytes * 8 {
                os.pos = pos0;
                os.bitbuf = bitbuf0;
                os.bitcount = bitcount0;
                os.overflow = false;
                Self::write_uncompressed(os, block, is_final);
            }
            begin = end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CompressionLevel, Compressor};
    use super::WHOLE_COUNT_MAX;
    use alloc::vec;
    use alloc::vec::Vec;

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
        for n in [1, 7, 8, 9, 15, 16, 17, 63, 64, 65, 300, 1000] {
            v.push(noise(n, n as u64));
            v.push(vec![0u8; n]);
            v.push(vec![9u8; n]);
        }
        // Zero runs of every length 1..=300 between literals, counted whole
        // (all length symbols), and 1..=600, sampled.
        for max in [300usize, 600] {
            let mut runs = Vec::new();
            for n in 1..=max {
                runs.push(1 + (n % 250) as u8);
                runs.extend(core::iter::repeat_n(0u8, n));
            }
            v.push(runs);
        }
        // Few distinct literals: most codes absent from a counted table.
        let few = noise(20_000, 9)
            .iter()
            .map(|&r| [0, 0, 1, 255][r as usize % 4])
            .collect();
        v.push(few);
        // Both sides of the whole-count limit.
        for n in [WHOLE_COUNT_MAX, WHOLE_COUNT_MAX + 1] {
            v.push(
                noise(n, 5)
                    .iter()
                    .map(|&r| (r % 9).wrapping_sub(4))
                    .collect(),
            );
        }
        v.push(vec![0u8; 1_000_000]);
        v.push(noise(700_000, 3)); // incompressible: stored fallback
        // Filtered-image-like: small residuals with zero stretches.
        let n = noise(900_000, 7);
        v.push(
            n.iter()
                .enumerate()
                .map(|(i, &r)| {
                    if (i / 3000) % 3 == 0 {
                        0
                    } else {
                        (r % 7).wrapping_sub(3)
                    }
                })
                .collect(),
        );
        v
    }

    /// Inputs on both sides of the size thresholds exercise the counted and
    /// sampled tables and the single and pair LUTs.
    #[test]
    fn ultra_roundtrip() {
        for data in &inputs() {
            roundtrip(CompressionLevel::png(1), data);
        }
    }

    #[test]
    fn ultra_never_expands_past_stored() {
        for n in [5_000, 600_000] {
            let data = noise(n, 11);
            let out = roundtrip(CompressionLevel::png(1), &data);
            assert!(out <= data.len() + data.len() / 1000 + 64, "{n}: {out}");
        }
    }

    /// Inputs whose coded size crosses the stored size one step at a time:
    /// every output must fit `zlib_compress_bound`, which is tight (stored
    /// size exactly) below 5000 bytes.
    #[test]
    fn ultra_fits_bound_at_stored_crossover() {
        for n in [600usize, 4999] {
            let mut data: Vec<u8> = noise(n, n as u64).iter().map(|&b| b | 1).collect();
            for k in 0..n / 2 {
                data[2 * k + 1] = 0; // isolated zeros: ~1 byte cheaper per step
                roundtrip(CompressionLevel::png(1), &data);
            }
        }
    }

    #[cfg(feature = "threads")]
    #[test]
    fn ultra_parallel_gzip() {
        // 2 MB (sampled chunks) and 200 KB (counted chunks) over 4 threads.
        for len in [2_000_000, 200_000] {
            let data: Vec<u8> = noise(len, 5).iter().map(|&r| r % 5).collect();
            let mut c = Compressor::new(CompressionLevel::png(1));
            let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len()) + 4096];
            let n = c
                .gzip_compress_parallel(&data, &mut out, 4, enough::Unstoppable)
                .unwrap();
            let mut back = vec![0u8; data.len()];
            let got = crate::Decompressor::new()
                .gzip_decompress(&out[..n], &mut back, enough::Unstoppable)
                .unwrap();
            assert_eq!(got.output_written, data.len());
            assert!(back == data, "{len}: parallel mismatch");
        }
    }
}

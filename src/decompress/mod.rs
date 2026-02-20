//! DEFLATE decompression, ported from libdeflate's deflate_decompress.c and
//! decompress_template.h.

#[cfg(feature = "alloc")]
pub mod streaming;

use crate::checksum;
use crate::error::DecompressionError;

// ---------------------------------------------------------------------------
// Decode table constants
// ---------------------------------------------------------------------------

pub(crate) const PRECODE_TABLEBITS: u32 = 7;
const PRECODE_ENOUGH: usize = 128;
pub(crate) const LITLEN_TABLEBITS: u32 = 11;
const LITLEN_ENOUGH: usize = 2342;
pub(crate) const OFFSET_TABLEBITS: u32 = 8;
const OFFSET_ENOUGH: usize = 402;

// Decode table entry flags
pub(crate) const HUFFDEC_LITERAL: u32 = 0x8000_0000;
pub(crate) const HUFFDEC_EXCEPTIONAL: u32 = 0x0000_8000;
pub(crate) const HUFFDEC_SUBTABLE_POINTER: u32 = 0x0000_4000;
pub(crate) const HUFFDEC_END_OF_BLOCK: u32 = 0x0000_2000;

// Bitstream constants (64-bit)
pub(crate) const CONSUMABLE_NBITS: u32 = 56; // MAX_BITSLEFT(63) - 7

// Fastloop safety margins — how many bytes the fastloop can read/write per iteration.
// Max bytes that can be written past the nominal match end in one fastloop iteration.
// Word copies (8 bytes) can overrun by at most 7 bytes; RLE uses fill() (exact length).
pub(crate) const FASTLOOP_MAX_BYTES_WRITTEN: usize =
    2 + crate::constants::DEFLATE_MAX_MATCH_LEN as usize + 7;
// Input: worst-case bytes consumed per iteration + 8-byte read-ahead for branchless refill
pub(crate) const FASTLOOP_MAX_BYTES_READ: usize = 32;

// DEFLATE format constants (local copies for internal use)
pub(crate) const DEFLATE_BLOCKTYPE_UNCOMPRESSED: u32 = 0;
pub(crate) const DEFLATE_BLOCKTYPE_STATIC_HUFFMAN: u32 = 1;
pub(crate) const DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN: u32 = 2;
pub(crate) const DEFLATE_NUM_PRECODE_SYMS: usize = 19;
pub(crate) const DEFLATE_NUM_LITLEN_SYMS: usize = 288;
pub(crate) const DEFLATE_NUM_OFFSET_SYMS: usize = 32;
const DEFLATE_MAX_NUM_SYMS: usize = 288;
const DEFLATE_MAX_CODEWORD_LEN: usize = 15;
pub(crate) const DEFLATE_MAX_PRE_CODEWORD_LEN: u32 = 7;
const DEFLATE_MAX_LENS_OVERRUN: usize = 137;

pub(crate) const DEFLATE_PRECODE_LENS_PERMUTATION: [u8; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

// gzip constants
const GZIP_FOOTER_SIZE: usize = 8;
const GZIP_MIN_OVERHEAD: usize = 10 + GZIP_FOOTER_SIZE;
pub(crate) const GZIP_ID1: u8 = 0x1F;
pub(crate) const GZIP_ID2: u8 = 0x8B;
pub(crate) const GZIP_CM_DEFLATE: u8 = 8;
pub(crate) const GZIP_FHCRC: u8 = 0x02;
pub(crate) const GZIP_FEXTRA: u8 = 0x04;
pub(crate) const GZIP_FNAME: u8 = 0x08;
pub(crate) const GZIP_FCOMMENT: u8 = 0x10;
pub(crate) const GZIP_FRESERVED: u8 = 0xE0;

// zlib constants
const ZLIB_FOOTER_SIZE: usize = 4;
const ZLIB_MIN_OVERHEAD: usize = 2 + ZLIB_FOOTER_SIZE;
pub(crate) const ZLIB_CM_DEFLATE: u8 = 8;
pub(crate) const ZLIB_CINFO_32K_WINDOW: u8 = 7;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline(always)]
pub(crate) fn bitmask(n: u32) -> u64 {
    (1u64 << n) - 1
}

#[inline(always)]
fn bsr32(v: u32) -> u32 {
    debug_assert!(v != 0);
    31 - v.leading_zeros()
}

/// Extract variable number of bits from word. count must be < 64.
#[inline(always)]
pub(crate) fn extract_varbits(word: u64, count: u32) -> u64 {
    word & bitmask(count)
}

/// Extract variable bits using the low byte of `entry` as the count.
#[inline(always)]
pub(crate) fn extract_varbits8(word: u64, entry: u32) -> u64 {
    word & bitmask(entry & 0xFF)
}

#[inline(always)]
fn make_decode_table_entry(decode_results: &[u32], sym: u32, len: u32) -> u32 {
    decode_results[sym as usize] + (len << 8) + len
}

// ---------------------------------------------------------------------------
// Decode result tables (generated at compile time)
// ---------------------------------------------------------------------------

const fn gen_precode_decode_results() -> [u32; DEFLATE_NUM_PRECODE_SYMS] {
    let mut r = [0u32; DEFLATE_NUM_PRECODE_SYMS];
    let mut i = 0;
    while i < DEFLATE_NUM_PRECODE_SYMS {
        r[i] = (i as u32) << 16;
        i += 1;
    }
    r
}

const fn gen_litlen_decode_results() -> [u32; DEFLATE_NUM_LITLEN_SYMS] {
    let mut r = [0u32; DEFLATE_NUM_LITLEN_SYMS];
    // Literals 0-255
    let mut i = 0;
    while i < 256 {
        r[i] = HUFFDEC_LITERAL | ((i as u32) << 16);
        i += 1;
    }
    // End of block (symbol 256)
    r[256] = HUFFDEC_EXCEPTIONAL | HUFFDEC_END_OF_BLOCK;
    // Lengths (symbols 257-285)
    let bases: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    let extra: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    i = 0;
    while i < 29 {
        r[257 + i] = ((bases[i] as u32) << 16) | (extra[i] as u32);
        i += 1;
    }
    // Symbols 286-287: unused but filled same as 285
    r[286] = 258u32 << 16;
    r[287] = 258u32 << 16;
    r
}

const fn gen_offset_decode_results() -> [u32; DEFLATE_NUM_OFFSET_SYMS] {
    let mut r = [0u32; DEFLATE_NUM_OFFSET_SYMS];
    let bases: [u32; 32] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577, 24577, 24577,
    ];
    let extra: [u8; 32] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13, 13, 13,
    ];
    let mut i = 0;
    while i < DEFLATE_NUM_OFFSET_SYMS {
        r[i] = (bases[i] << 16) | (extra[i] as u32);
        i += 1;
    }
    r
}

pub(crate) static PRECODE_DECODE_RESULTS: [u32; DEFLATE_NUM_PRECODE_SYMS] =
    gen_precode_decode_results();
pub(crate) static LITLEN_DECODE_RESULTS: [u32; DEFLATE_NUM_LITLEN_SYMS] =
    gen_litlen_decode_results();
pub(crate) static OFFSET_DECODE_RESULTS: [u32; DEFLATE_NUM_OFFSET_SYMS] =
    gen_offset_decode_results();

// ---------------------------------------------------------------------------
// Decompressor struct
// ---------------------------------------------------------------------------

const LENS_SIZE: usize =
    DEFLATE_NUM_LITLEN_SYMS + DEFLATE_NUM_OFFSET_SYMS + DEFLATE_MAX_LENS_OVERRUN;

/// DEFLATE/zlib/gzip decompressor.
///
/// Reusable across multiple decompression calls. Caches static Huffman
/// decode tables between calls for efficiency.
///
/// ```
/// use zenflate::{Compressor, CompressionLevel, Decompressor};
///
/// // Compress some data
/// let data = b"The quick brown fox jumps over the lazy dog.";
/// let mut c = Compressor::new(CompressionLevel::fastest());
/// let bound = Compressor::deflate_compress_bound(data.len());
/// let mut compressed = vec![0u8; bound];
/// let csize = c.deflate_compress(data, &mut compressed).unwrap();
///
/// // Decompress it back
/// let mut d = Decompressor::new();
/// let mut output = vec![0u8; data.len()];
/// let dsize = d.deflate_decompress(&compressed[..csize], &mut output).unwrap();
/// assert_eq!(&output[..dsize], &data[..]);
/// ```
pub struct Decompressor {
    pub(crate) precode_lens: [u8; DEFLATE_NUM_PRECODE_SYMS],
    pub(crate) precode_decode_table: [u32; PRECODE_ENOUGH],
    pub(crate) lens: [u8; LENS_SIZE],
    pub(crate) litlen_decode_table: [u32; LITLEN_ENOUGH],
    pub(crate) offset_decode_table: [u32; OFFSET_ENOUGH],
    pub(crate) sorted_syms: [u16; DEFLATE_MAX_NUM_SYMS],
    pub(crate) static_codes_loaded: bool,
    pub(crate) litlen_tablebits: u32,
}

impl Default for Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor {
    /// Create a new decompressor.
    pub fn new() -> Self {
        Self {
            precode_lens: [0; DEFLATE_NUM_PRECODE_SYMS],
            precode_decode_table: [0; PRECODE_ENOUGH],
            lens: [0; LENS_SIZE],
            litlen_decode_table: [0; LITLEN_ENOUGH],
            offset_decode_table: [0; OFFSET_ENOUGH],
            sorted_syms: [0; DEFLATE_MAX_NUM_SYMS],
            static_codes_loaded: false,
            litlen_tablebits: 0,
        }
    }

    /// Decompress raw DEFLATE data. Returns the number of output bytes written.
    pub fn deflate_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DecompressionError> {
        let (_in_consumed, out_written) = self.deflate_decompress_core(input, output)?;
        Ok(out_written)
    }

    /// Decompress raw DEFLATE data, returning `(input_consumed, output_written)`.
    ///
    /// Like [`deflate_decompress`](Self::deflate_decompress), but also reports how many
    /// bytes of `input` were consumed. Useful when the compressed data is embedded in a
    /// larger buffer and you need to know where it ends.
    pub fn deflate_decompress_ex(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), DecompressionError> {
        self.deflate_decompress_core(input, output)
    }

    /// Decompress zlib-wrapped data. Returns the number of output bytes written.
    pub fn zlib_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DecompressionError> {
        if input.len() < ZLIB_MIN_OVERHEAD {
            return Err(DecompressionError::BadData);
        }
        // 2-byte header (big-endian)
        let hdr = u16::from_be_bytes([input[0], input[1]]);
        // FCHECK
        if !hdr.is_multiple_of(31) {
            return Err(DecompressionError::BadData);
        }
        // CM (low 4 bits of CMF = first byte)
        if (input[0] & 0xF) != ZLIB_CM_DEFLATE {
            return Err(DecompressionError::BadData);
        }
        // CINFO (high 4 bits of CMF)
        if (input[0] >> 4) > ZLIB_CINFO_32K_WINDOW {
            return Err(DecompressionError::BadData);
        }
        // FDICT (bit 5 of FLG = second byte)
        if (input[1] >> 5) & 1 != 0 {
            return Err(DecompressionError::BadData);
        }

        let deflate_data = &input[2..input.len() - ZLIB_FOOTER_SIZE];
        let (in_consumed, out_written) = self.deflate_decompress_core(deflate_data, output)?;

        // Verify Adler-32 (big-endian, after DEFLATE data)
        let footer_start = 2 + in_consumed;
        let expected = u32::from_be_bytes([
            input[footer_start],
            input[footer_start + 1],
            input[footer_start + 2],
            input[footer_start + 3],
        ]);
        let actual = checksum::adler32(1, &output[..out_written]);
        if actual != expected {
            return Err(DecompressionError::BadData);
        }
        Ok(out_written)
    }

    /// Decompress gzip-wrapped data. Returns the number of output bytes written.
    pub fn gzip_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DecompressionError> {
        if input.len() < GZIP_MIN_OVERHEAD {
            return Err(DecompressionError::BadData);
        }
        let mut pos = 0;
        // ID1, ID2
        if input[pos] != GZIP_ID1 || input[pos + 1] != GZIP_ID2 {
            return Err(DecompressionError::BadData);
        }
        pos += 2;
        // CM
        if input[pos] != GZIP_CM_DEFLATE {
            return Err(DecompressionError::BadData);
        }
        pos += 1;
        let flg = input[pos];
        pos += 1;
        // MTIME(4) + XFL(1) + OS(1) = 6 bytes
        pos += 6;

        if flg & GZIP_FRESERVED != 0 {
            return Err(DecompressionError::BadData);
        }

        // Extra field
        if flg & GZIP_FEXTRA != 0 {
            if pos + 2 > input.len() {
                return Err(DecompressionError::BadData);
            }
            let xlen = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
            pos += 2;
            if input.len() - pos < xlen + GZIP_FOOTER_SIZE {
                return Err(DecompressionError::BadData);
            }
            pos += xlen;
        }

        // Original file name (zero terminated)
        if flg & GZIP_FNAME != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            pos += 1; // skip the zero terminator
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(DecompressionError::BadData);
            }
        }

        // File comment (zero terminated)
        if flg & GZIP_FCOMMENT != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            pos += 1;
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(DecompressionError::BadData);
            }
        }

        // CRC16 for gzip header
        if flg & GZIP_FHCRC != 0 {
            pos += 2;
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(DecompressionError::BadData);
            }
        }

        // Compressed DEFLATE data
        let deflate_end = input.len() - GZIP_FOOTER_SIZE;
        if pos > deflate_end {
            return Err(DecompressionError::BadData);
        }
        let (in_consumed, out_written) =
            self.deflate_decompress_core(&input[pos..deflate_end], output)?;

        let footer_start = pos + in_consumed;

        // CRC32 (little-endian)
        let expected_crc = u32::from_le_bytes([
            input[footer_start],
            input[footer_start + 1],
            input[footer_start + 2],
            input[footer_start + 3],
        ]);
        if checksum::crc32(0, &output[..out_written]) != expected_crc {
            return Err(DecompressionError::BadData);
        }

        // ISIZE (little-endian, mod 2^32)
        let expected_size = u32::from_le_bytes([
            input[footer_start + 4],
            input[footer_start + 5],
            input[footer_start + 6],
            input[footer_start + 7],
        ]);
        if (out_written as u32) != expected_size {
            return Err(DecompressionError::BadData);
        }

        Ok(out_written)
    }
}

// ---------------------------------------------------------------------------
// Bitstream refill
// ---------------------------------------------------------------------------

/// Refill the bitbuffer to have at least CONSUMABLE_NBITS (56) bits.
/// Uses branchless word refill when 8 bytes are available, otherwise
/// falls back to byte-at-a-time with overread tracking.
#[inline(always)]
pub(crate) fn refill_bits(
    bitbuf: &mut u64,
    bitsleft: &mut u32,
    input: &[u8],
    in_pos: &mut usize,
    overread_count: &mut usize,
) -> Result<(), DecompressionError> {
    if *in_pos + 8 <= input.len() {
        // Branchless refill: read 8 bytes, merge, advance by consumed bytes
        let word = crate::fast_bytes::load_u64_le(input, *in_pos);
        *bitbuf |= word << *bitsleft;
        *in_pos += 7 - ((*bitsleft as usize >> 3) & 7);
        *bitsleft |= 56; // MAX_BITSLEFT & !7
    } else {
        // Byte-at-a-time fallback near end of input
        while *bitsleft < CONSUMABLE_NBITS {
            if *in_pos < input.len() {
                *bitbuf |= (input[*in_pos] as u64) << *bitsleft;
                *in_pos += 1;
            } else {
                *overread_count += 1;
                if *overread_count > 8 {
                    return Err(DecompressionError::BadData);
                }
            }
            *bitsleft += 8;
        }
    }
    Ok(())
}

/// Branchless bitstream refill for the fastloop.
///
/// Same as the hot path of `refill_bits`, but without the end-of-input check
/// or overread tracking. Only safe to call when `in_pos + 8 <= input.len()`.
#[inline(always)]
pub(crate) fn refill_bits_fast(
    bitbuf: &mut u64,
    bitsleft: &mut u32,
    input: &[u8],
    in_pos: &mut usize,
) {
    let word = crate::fast_bytes::load_u64_le(input, *in_pos);
    *bitbuf |= word << *bitsleft;
    *in_pos += 7 - ((*bitsleft as usize >> 3) & 7);
    *bitsleft |= 56;
}

/// Copy an 8-byte word within the output buffer (for stride-by-offset match copies).
/// The read completes before the write, so overlapping src/dst ranges are safe.
#[inline(always)]
fn copy_word(output: &mut [u8], src: usize, dst: usize) {
    let mut tmp = [0u8; 8];
    tmp.copy_from_slice(&output[src..src + 8]);
    output[dst..dst + 8].copy_from_slice(&tmp);
}

/// Look up a decode table entry by index.
#[inline(always)]
pub(crate) fn table_lookup(table: &[u32], idx: u64) -> u32 {
    table[idx as usize]
}

/// Store a literal byte to the output buffer.
#[inline(always)]
fn store_lit(output: &mut [u8], pos: usize, byte: u8) {
    output[pos] = byte;
}

/// Read a single byte from the output buffer (for match copy source).
#[inline(always)]
fn load_byte(output: &[u8], pos: usize) -> u8 {
    output[pos]
}

/// Forward match copy in the fastloop. Handles all overlap cases.
/// Uses safe indexing everywhere — benchmarking showed that `get_unchecked`
/// paths actually regress 5-6% on mixed/photo data because LLVM loses
/// bounds information that enables better optimization.
#[inline(always)]
pub(crate) fn fastloop_match_copy(
    output: &mut [u8],
    out_pos: usize,
    src_start: usize,
    length: usize,
    offset: usize,
) {
    // Unified path for safe and unchecked modes. The `copy_word` function
    // already uses raw pointers when `unchecked` is enabled.
    // Keeping a single code path avoids instruction cache bloat from duplicated
    // match copy logic (benchmarked: separate unchecked paths regressed 6%).
    let end = out_pos + length;
    if offset >= length {
        // Non-overlapping: memcpy via copy_within (SIMD-optimized in libc)
        output.copy_within(src_start..src_start + length, out_pos);
    } else if offset == 1 {
        // RLE: fill with repeated byte (memset, SIMD-optimized in libc)
        let byte = load_byte(output, src_start);
        output[out_pos..end].fill(byte);
    } else if offset < 8 {
        // Small offset (2-7): stride-by-offset word copy
        let work = &mut output[src_start..end + 8];
        let mut d = offset;
        while d + 8 <= work.len() {
            copy_word(work, d - offset, d);
            d += offset;
        }
    } else {
        // Overlapping with offset >= 8: copy first `offset` bytes, then forward
        output.copy_within(src_start..src_start + offset, out_pos);
        for i in offset..length {
            output[out_pos + i] = output[src_start + i];
        }
    }
}

// ---------------------------------------------------------------------------
// build_decode_table
// ---------------------------------------------------------------------------

/// Build a Huffman decode table from codeword lengths.
///
/// Returns true on success, false if the lengths don't form a valid code.
/// Faithfully ported from libdeflate's build_decode_table().
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_decode_table(
    decode_table: &mut [u32],
    lens: &[u8],
    num_syms: usize,
    decode_results: &[u32],
    mut table_bits: u32,
    mut max_codeword_len: u32,
    sorted_syms: &mut [u16],
    table_bits_ret: Option<&mut u32>,
) -> bool {
    let mut len_counts = [0u32; DEFLATE_MAX_CODEWORD_LEN + 1];
    let mut offsets = [0u32; DEFLATE_MAX_CODEWORD_LEN + 1];

    // Count codewords of each length
    for i in 0..num_syms {
        len_counts[lens[i] as usize] += 1;
    }

    // Determine actual max codeword length
    while max_codeword_len > 1 && len_counts[max_codeword_len as usize] == 0 {
        max_codeword_len -= 1;
    }
    if let Some(ret) = table_bits_ret {
        table_bits = table_bits.min(max_codeword_len);
        *ret = table_bits;
    }

    // Sort symbols by codeword length; also compute codespace_used
    offsets[0] = 0;
    offsets[1] = len_counts[0];
    let mut codespace_used: u32 = 0;
    for len in 1..max_codeword_len as usize {
        offsets[len + 1] = offsets[len] + len_counts[len];
        codespace_used = (codespace_used << 1) + len_counts[len];
    }
    codespace_used = (codespace_used << 1) + len_counts[max_codeword_len as usize];

    for (sym, &cw_len) in lens.iter().enumerate().take(num_syms) {
        let l = cw_len as usize;
        sorted_syms[offsets[l] as usize] = sym as u16;
        offsets[l] += 1;
    }

    let skip_unused = offsets[0] as usize;
    let mut sorted_pos = skip_unused;

    let full_codespace = 1u32 << max_codeword_len;

    // Overfull code?
    if codespace_used > full_codespace {
        return false;
    }

    // Incomplete code?
    if codespace_used < full_codespace {
        let sym = if codespace_used == 0 {
            0u32 // arbitrary
        } else {
            if codespace_used != (1u32 << (max_codeword_len - 1)) || len_counts[1] != 1 {
                return false;
            }
            sorted_syms[sorted_pos] as u32
        };
        let entry = make_decode_table_entry(decode_results, sym, 1);
        decode_table[..(1usize << table_bits)].fill(entry);
        return true;
    }

    // Complete code. Fill main table entries with incremental doubling.
    let mut codeword: u32 = 0;
    let mut len: u32 = 1;
    while len_counts[len as usize] == 0 {
        len += 1;
    }
    let mut count = len_counts[len as usize];
    let mut cur_table_end: u32 = 1u32 << len;

    while len <= table_bits {
        loop {
            decode_table[codeword as usize] =
                make_decode_table_entry(decode_results, sorted_syms[sorted_pos] as u32, len);
            sorted_pos += 1;

            if codeword == cur_table_end - 1 {
                // Last codeword (all 1's) — double table to fill remaining
                while len < table_bits {
                    decode_table.copy_within(0..cur_table_end as usize, cur_table_end as usize);
                    cur_table_end <<= 1;
                    len += 1;
                }
                return true;
            }

            // Advance to next codeword (bit-reversed increment)
            let bit = 1u32 << bsr32(codeword ^ (cur_table_end - 1));
            codeword &= bit - 1;
            codeword |= bit;

            count -= 1;
            if count == 0 {
                break;
            }
        }

        // Advance to next codeword length
        loop {
            len += 1;
            if len <= table_bits {
                decode_table.copy_within(0..cur_table_end as usize, cur_table_end as usize);
                cur_table_end <<= 1;
            }
            count = len_counts[len as usize];
            if count != 0 {
                break;
            }
        }
    }

    // Process codewords with len > table_bits (subtables)
    cur_table_end = 1u32 << table_bits;
    let mut subtable_prefix: u32 = u32::MAX;
    let mut subtable_start: u32 = 0;

    loop {
        let prefix = codeword & ((1u32 << table_bits) - 1);
        if prefix != subtable_prefix {
            subtable_prefix = prefix;
            subtable_start = cur_table_end;

            let mut subtable_bits = len - table_bits;
            let mut codespace = count;
            while codespace < (1u32 << subtable_bits) {
                subtable_bits += 1;
                codespace = (codespace << 1) + len_counts[(table_bits + subtable_bits) as usize];
            }
            cur_table_end = subtable_start + (1u32 << subtable_bits);

            decode_table[subtable_prefix as usize] = (subtable_start << 16)
                | HUFFDEC_EXCEPTIONAL
                | HUFFDEC_SUBTABLE_POINTER
                | (subtable_bits << 8)
                | table_bits;
        }

        let entry = make_decode_table_entry(
            decode_results,
            sorted_syms[sorted_pos] as u32,
            len - table_bits,
        );
        sorted_pos += 1;

        let stride = 1u32 << (len - table_bits);
        let mut i = subtable_start + (codeword >> table_bits);
        while i < cur_table_end {
            decode_table[i as usize] = entry;
            i += stride;
        }

        // Advance to next codeword
        if codeword == (1u32 << len) - 1 {
            return true; // last codeword
        }
        let bit = 1u32 << bsr32(codeword ^ ((1u32 << len) - 1));
        codeword &= bit - 1;
        codeword |= bit;
        count -= 1;
        while count == 0 {
            len += 1;
            count = len_counts[len as usize];
        }
    }
}

// ---------------------------------------------------------------------------
// Core DEFLATE decompression (generic loop only — no fastloop yet)
// ---------------------------------------------------------------------------

impl Decompressor {
    fn deflate_decompress_core(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), DecompressionError> {
        let mut in_pos: usize = 0;
        let mut out_pos: usize = 0;
        let mut bitbuf: u64 = 0;
        let mut bitsleft: u32 = 0;
        let mut overread_count: usize = 0;

        let bad = DecompressionError::BadData;
        let no_space = DecompressionError::InsufficientSpace;

        loop {
            // --- Read block header ---
            refill_bits(
                &mut bitbuf,
                &mut bitsleft,
                input,
                &mut in_pos,
                &mut overread_count,
            )?;

            let is_final = (bitbuf & 1) != 0;
            let block_type = ((bitbuf >> 1) & 3) as u32;

            if block_type == DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN {
                // --- Dynamic Huffman block ---
                let num_litlen_syms = 257 + ((bitbuf >> 3) & bitmask(5)) as usize;
                let num_offset_syms = 1 + ((bitbuf >> 8) & bitmask(5)) as usize;
                let num_explicit_precode_lens = 4 + ((bitbuf >> 13) & bitmask(4)) as usize;

                self.static_codes_loaded = false;

                // First precode len is packed with the header
                self.precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[0] as usize] =
                    ((bitbuf >> 17) & 7) as u8;
                bitbuf >>= 20;
                bitsleft -= 20;

                refill_bits(
                    &mut bitbuf,
                    &mut bitsleft,
                    input,
                    &mut in_pos,
                    &mut overread_count,
                )?;

                // Remaining precode lens (3 bits each, max 18 more)
                for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION[1..num_explicit_precode_lens] {
                    self.precode_lens[perm as usize] = (bitbuf & 7) as u8;
                    bitbuf >>= 3;
                    bitsleft -= 3;
                }
                for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION
                    [num_explicit_precode_lens..DEFLATE_NUM_PRECODE_SYMS]
                {
                    self.precode_lens[perm as usize] = 0;
                }

                // Build precode decode table
                if !build_decode_table(
                    &mut self.precode_decode_table,
                    &self.precode_lens,
                    DEFLATE_NUM_PRECODE_SYMS,
                    &PRECODE_DECODE_RESULTS,
                    PRECODE_TABLEBITS,
                    DEFLATE_MAX_PRE_CODEWORD_LEN,
                    &mut self.sorted_syms,
                    None,
                ) {
                    return Err(bad);
                }

                // Decode litlen + offset codeword lengths
                let total_syms = num_litlen_syms + num_offset_syms;
                let mut i = 0usize;
                while i < total_syms {
                    if bitsleft < DEFLATE_MAX_PRE_CODEWORD_LEN + 7 {
                        refill_bits(
                            &mut bitbuf,
                            &mut bitsleft,
                            input,
                            &mut in_pos,
                            &mut overread_count,
                        )?;
                    }

                    let entry = self.precode_decode_table
                        [(bitbuf & bitmask(DEFLATE_MAX_PRE_CODEWORD_LEN)) as usize];
                    bitbuf >>= (entry & 0xFF) as u64;
                    bitsleft -= entry & 0xFF;
                    let presym = (entry >> 16) as usize;

                    if presym < 16 {
                        self.lens[i] = presym as u8;
                        i += 1;
                        continue;
                    }

                    if presym == 16 {
                        // Repeat previous 3-6 times
                        if i == 0 {
                            return Err(bad);
                        }
                        let rep_val = self.lens[i - 1];
                        let rep_count = 3 + (bitbuf & 3) as usize;
                        bitbuf >>= 2;
                        bitsleft -= 2;
                        // Write up to 6 (safe: lens has overrun space)
                        for j in 0..6 {
                            self.lens[i + j] = rep_val;
                        }
                        i += rep_count;
                    } else if presym == 17 {
                        // Repeat zero 3-10 times
                        let rep_count = 3 + (bitbuf & 7) as usize;
                        bitbuf >>= 3;
                        bitsleft -= 3;
                        for j in 0..10 {
                            self.lens[i + j] = 0;
                        }
                        i += rep_count;
                    } else {
                        // presym == 18: repeat zero 11-138 times
                        let rep_count = 11 + (bitbuf & bitmask(7)) as usize;
                        bitbuf >>= 7;
                        bitsleft -= 7;
                        self.lens[i..i + rep_count].fill(0);
                        i += rep_count;
                    }
                }

                if i != total_syms {
                    return Err(bad);
                }

                // Build offset table first (uses lens[num_litlen_syms..])
                if !build_decode_table(
                    &mut self.offset_decode_table,
                    &self.lens[num_litlen_syms..],
                    num_offset_syms,
                    &OFFSET_DECODE_RESULTS,
                    OFFSET_TABLEBITS,
                    15,
                    &mut self.sorted_syms,
                    None,
                ) {
                    return Err(bad);
                }
                // Build litlen table (may overwrite lens via aliasing in C,
                // but in Rust they're separate arrays so no issue)
                if !build_decode_table(
                    &mut self.litlen_decode_table,
                    &self.lens,
                    num_litlen_syms,
                    &LITLEN_DECODE_RESULTS,
                    LITLEN_TABLEBITS,
                    15,
                    &mut self.sorted_syms,
                    Some(&mut self.litlen_tablebits),
                ) {
                    return Err(bad);
                }
            } else if block_type == DEFLATE_BLOCKTYPE_UNCOMPRESSED {
                // --- Uncompressed block ---
                bitsleft -= 3;

                // Align to byte boundary: rewind input past unconsumed bytes
                let extra_bytes = (bitsleft / 8) as usize;
                if overread_count > extra_bytes {
                    return Err(bad);
                }
                in_pos -= extra_bytes - overread_count;
                overread_count = 0;
                bitbuf = 0;
                bitsleft = 0;

                // Read LEN and NLEN
                if in_pos + 4 > input.len() {
                    return Err(bad);
                }
                let len = u16::from_le_bytes([input[in_pos], input[in_pos + 1]]) as usize;
                let nlen = u16::from_le_bytes([input[in_pos + 2], input[in_pos + 3]]);
                in_pos += 4;

                if len != (!nlen) as usize {
                    return Err(bad);
                }
                if len > output.len() - out_pos {
                    return Err(no_space);
                }
                if len > input.len() - in_pos {
                    return Err(bad);
                }

                output[out_pos..out_pos + len].copy_from_slice(&input[in_pos..in_pos + len]);
                in_pos += len;
                out_pos += len;

                if is_final {
                    break;
                }
                continue;
            } else if block_type == DEFLATE_BLOCKTYPE_STATIC_HUFFMAN {
                // --- Static Huffman block ---
                bitbuf >>= 3;
                bitsleft -= 3;

                if !self.static_codes_loaded {
                    self.static_codes_loaded = true;

                    // Fixed literal/length code lengths (RFC 1951 section 3.2.6)
                    for i in 0..144 {
                        self.lens[i] = 8;
                    }
                    for i in 144..256 {
                        self.lens[i] = 9;
                    }
                    for i in 256..280 {
                        self.lens[i] = 7;
                    }
                    for i in 280..288 {
                        self.lens[i] = 8;
                    }
                    // Fixed offset code: all 5 bits
                    for i in 288..320 {
                        self.lens[i] = 5;
                    }

                    if !build_decode_table(
                        &mut self.offset_decode_table,
                        &self.lens[288..],
                        32,
                        &OFFSET_DECODE_RESULTS,
                        OFFSET_TABLEBITS,
                        15,
                        &mut self.sorted_syms,
                        None,
                    ) {
                        return Err(bad);
                    }
                    if !build_decode_table(
                        &mut self.litlen_decode_table,
                        &self.lens,
                        288,
                        &LITLEN_DECODE_RESULTS,
                        LITLEN_TABLEBITS,
                        15,
                        &mut self.sorted_syms,
                        Some(&mut self.litlen_tablebits),
                    ) {
                        return Err(bad);
                    }
                }
            } else {
                return Err(bad);
            }

            // --- Fastloop + generic decode loop (literals and matches) ---
            let litlen_tablemask = bitmask(self.litlen_tablebits);
            let in_fastloop_end = input.len().saturating_sub(FASTLOOP_MAX_BYTES_READ);
            let out_fastloop_end = output.len().saturating_sub(FASTLOOP_MAX_BYTES_WRITTEN);

            // The fastloop processes the bulk of data without per-item bounds
            // checks. It exits when input/output margins are exhausted or
            // end-of-block is reached. The generic loop handles the remainder.
            let mut block_done = false;

            if in_pos < in_fastloop_end && out_pos < out_fastloop_end {
                // Initial refill and preload
                refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                let mut entry = table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);

                'fastloop: loop {
                    // Consume entry bits
                    let mut saved_bitbuf = bitbuf;
                    bitbuf >>= (entry & 0xFF) as u64;
                    bitsleft -= entry & 0xFF;

                    // --- Fast literal path: decode up to 3 literals ---
                    if entry & HUFFDEC_LITERAL != 0 {
                        // 1st literal (the primary item)
                        let lit = (entry >> 16) as u8;
                        entry = table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                        saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;
                        store_lit(output, out_pos, lit);
                        out_pos += 1;

                        if entry & HUFFDEC_LITERAL != 0 {
                            // 2nd literal (extra)
                            let lit = (entry >> 16) as u8;
                            entry =
                                table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                            saved_bitbuf = bitbuf;
                            bitbuf >>= (entry & 0xFF) as u64;
                            bitsleft -= entry & 0xFF;
                            store_lit(output, out_pos, lit);
                            out_pos += 1;

                            if entry & HUFFDEC_LITERAL != 0 {
                                // 3rd literal (replaces primary for next iter)
                                store_lit(output, out_pos, (entry >> 16) as u8);
                                out_pos += 1;
                                entry = table_lookup(
                                    &self.litlen_decode_table,
                                    bitbuf & litlen_tablemask,
                                );
                                refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                                if in_pos < in_fastloop_end && out_pos < out_fastloop_end {
                                    continue 'fastloop;
                                }
                                break 'fastloop;
                            }
                        }
                        // Entry is now non-literal, fall through to handle it
                    }

                    // --- Exceptional: subtable or end-of-block ---
                    if entry & HUFFDEC_EXCEPTIONAL != 0 {
                        if entry & HUFFDEC_END_OF_BLOCK != 0 {
                            block_done = true;
                            break 'fastloop;
                        }
                        // Subtable lookup
                        entry = table_lookup(
                            &self.litlen_decode_table,
                            (entry >> 16) as u64 + extract_varbits(bitbuf, (entry >> 8) & 0x3F),
                        );
                        saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;

                        if entry & HUFFDEC_LITERAL != 0 {
                            // Literal from subtable
                            store_lit(output, out_pos, (entry >> 16) as u8);
                            out_pos += 1;
                            entry =
                                table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                            refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                            if in_pos < in_fastloop_end && out_pos < out_fastloop_end {
                                continue 'fastloop;
                            }
                            break 'fastloop;
                        }
                        if entry & HUFFDEC_END_OF_BLOCK != 0 {
                            block_done = true;
                            break 'fastloop;
                        }
                        // Length from subtable, fall through
                    }

                    // --- Decode match length ---
                    let length = (entry >> 16) as usize
                        + (extract_varbits8(saved_bitbuf, entry) >> ((entry >> 8) as u8 as u64))
                            as usize;

                    // --- Decode match offset ---
                    let mut oentry = table_lookup(
                        &self.offset_decode_table,
                        bitbuf & bitmask(OFFSET_TABLEBITS),
                    );
                    if oentry & HUFFDEC_EXCEPTIONAL != 0 {
                        bitbuf >>= OFFSET_TABLEBITS as u64;
                        bitsleft -= OFFSET_TABLEBITS;
                        oentry = table_lookup(
                            &self.offset_decode_table,
                            (oentry >> 16) as u64 + extract_varbits(bitbuf, (oentry >> 8) & 0x3F),
                        );
                    }
                    let saved_bitbuf_off = bitbuf;
                    bitbuf >>= (oentry & 0xFF) as u64;
                    bitsleft -= oentry & 0xFF;

                    let offset = (oentry >> 16) as usize
                        + (extract_varbits8(saved_bitbuf_off, oentry)
                            >> ((oentry >> 8) as u8 as u64)) as usize;

                    if offset == 0 || offset > out_pos {
                        return Err(bad);
                    }

                    // Preload next entry and refill BEFORE copy to overlap latency
                    entry = table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                    refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);

                    // Copy match data — dispatches to unchecked raw-pointer
                    // word copies or safe slice operations.
                    fastloop_match_copy(output, out_pos, out_pos - offset, length, offset);
                    out_pos += length;

                    if in_pos >= in_fastloop_end || out_pos >= out_fastloop_end {
                        break 'fastloop;
                    }
                }
            }

            // --- Generic decode loop (handles remainder after fastloop) ---
            if !block_done {
                loop {
                    refill_bits(
                        &mut bitbuf,
                        &mut bitsleft,
                        input,
                        &mut in_pos,
                        &mut overread_count,
                    )?;

                    let mut entry =
                        table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                    let mut saved_bitbuf = bitbuf;
                    bitbuf >>= (entry & 0xFF) as u64;
                    bitsleft -= entry & 0xFF;

                    // Resolve subtable if needed
                    if entry & HUFFDEC_SUBTABLE_POINTER != 0 {
                        entry = table_lookup(
                            &self.litlen_decode_table,
                            (entry >> 16) as u64 + extract_varbits(bitbuf, (entry >> 8) & 0x3F),
                        );
                        saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;
                    }

                    let value = entry >> 16;

                    // Literal?
                    if entry & HUFFDEC_LITERAL != 0 {
                        if out_pos >= output.len() {
                            return Err(no_space);
                        }
                        output[out_pos] = value as u8;
                        out_pos += 1;
                        continue;
                    }

                    // End of block?
                    if entry & HUFFDEC_END_OF_BLOCK != 0 {
                        break;
                    }

                    // Length: base + extra bits
                    let length = value as usize
                        + (extract_varbits8(saved_bitbuf, entry) >> ((entry >> 8) as u8 as u64))
                            as usize;

                    if length > output.len() - out_pos {
                        return Err(no_space);
                    }

                    // On 64-bit: CAN_CONSUME(48) is true, no refill needed here

                    // Decode offset
                    let mut oentry = table_lookup(
                        &self.offset_decode_table,
                        bitbuf & bitmask(OFFSET_TABLEBITS),
                    );
                    if oentry & HUFFDEC_EXCEPTIONAL != 0 {
                        bitbuf >>= OFFSET_TABLEBITS as u64;
                        bitsleft -= OFFSET_TABLEBITS;
                        oentry = table_lookup(
                            &self.offset_decode_table,
                            (oentry >> 16) as u64 + extract_varbits(bitbuf, (oentry >> 8) & 0x3F),
                        );
                    }
                    let saved_bitbuf_off = bitbuf;
                    bitbuf >>= (oentry & 0xFF) as u64;
                    bitsleft -= oentry & 0xFF;

                    let offset = (oentry >> 16) as usize
                        + (extract_varbits8(saved_bitbuf_off, oentry)
                            >> ((oentry >> 8) as u8 as u64)) as usize;

                    // Validate offset
                    if offset == 0 || offset > out_pos {
                        return Err(bad);
                    }

                    // Copy match data (may overlap when offset < length)
                    let src_start = out_pos - offset;
                    if offset >= length {
                        output.copy_within(src_start..src_start + length, out_pos);
                    } else if offset == 1 {
                        let byte = output[src_start];
                        output[out_pos..out_pos + length].fill(byte);
                    } else if length <= 32 {
                        for i in 0..length {
                            output[out_pos + i] = output[src_start + i];
                        }
                    } else {
                        output.copy_within(src_start..src_start + offset, out_pos);
                        let mut copied = offset;
                        while copied < length {
                            let chunk = copied.min(length - copied);
                            output.copy_within(out_pos..out_pos + chunk, out_pos + copied);
                            copied += chunk;
                        }
                    }
                    out_pos += length;
                }
            }

            if is_final {
                break;
            }
        }

        // Verify we didn't consume implicit zero bytes
        let final_bitsleft = bitsleft;
        if overread_count > (final_bitsleft / 8) as usize {
            return Err(bad);
        }

        // Compute actual input consumed
        let actual_in = in_pos - ((final_bitsleft / 8) as usize - overread_count);

        Ok((actual_in, out_pos))
    }
}

// All decompress tests use libdeflater (C FFI) to create test data.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn test_decompress_empty_static() {
        // Compress empty data with libdeflater, decompress with us
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(1).unwrap());
        let bound = c.deflate_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&[], &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; 0];
        let out_size = d
            .deflate_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, 0);
    }

    #[test]
    fn test_decompress_hello_world() {
        let data = b"Hello, World!";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .deflate_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(&output, data);
    }

    #[test]
    fn test_decompress_all_levels() {
        let data: Vec<u8> = (0..=255).cycle().take(10_000).collect();
        for level in 1..=12 {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
            let bound = c.deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();

            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .deflate_decompress(&compressed[..csize], &mut output)
                .unwrap();
            assert_eq!(out_size, data.len(), "level {level}");
            assert_eq!(output, data, "level {level}");
        }
    }

    #[test]
    fn test_decompress_all_zeros() {
        let data = vec![0u8; 100_000];
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .deflate_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(output, data);
    }

    #[test]
    fn test_decompress_uncompressed_block() {
        // Level 0 produces uncompressed blocks
        let data = b"Uncompressed block test data!";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(0).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .deflate_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(&output[..], data);
    }

    #[test]
    fn test_zlib_decompress() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(&data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .zlib_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(output, data);
    }

    #[test]
    fn test_gzip_decompress() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(&data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .gzip_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(output, data);
    }

    #[test]
    fn test_decompress_large() {
        let data: Vec<u8> = (0..=255).cycle().take(1_000_000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let out_size = d
            .deflate_decompress(&compressed[..csize], &mut output)
            .unwrap();
        assert_eq!(out_size, data.len());
        assert_eq!(output, data);
    }

    #[test]
    fn test_decompress_single_byte() {
        for b in 0..=255u8 {
            let data = [b];
            let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
            let bound = c.deflate_compress_bound(1);
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();

            let mut d = Decompressor::new();
            let mut output = vec![0u8; 1];
            let out_size = d
                .deflate_decompress(&compressed[..csize], &mut output)
                .unwrap();
            assert_eq!(out_size, 1);
            assert_eq!(output[0], b);
        }
    }

    #[test]
    fn test_all_formats_all_levels() {
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        for level in 0..=12 {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());

            // DEFLATE
            let bound = c.deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .deflate_decompress(&compressed[..csize], &mut output)
                .unwrap();
            assert_eq!(out_size, data.len(), "deflate level {level}");
            assert_eq!(output, data, "deflate level {level}");

            // zlib
            let bound = c.zlib_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.zlib_compress(&data, &mut compressed).unwrap();
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .zlib_decompress(&compressed[..csize], &mut output)
                .unwrap();
            assert_eq!(out_size, data.len(), "zlib level {level}");
            assert_eq!(output, data, "zlib level {level}");

            // gzip
            let bound = c.gzip_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.gzip_compress(&data, &mut compressed).unwrap();
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .gzip_decompress(&compressed[..csize], &mut output)
                .unwrap();
            assert_eq!(out_size, data.len(), "gzip level {level}");
            assert_eq!(output, data, "gzip level {level}");
        }
    }
}

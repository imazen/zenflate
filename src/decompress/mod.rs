//! DEFLATE decompression. Core decode tables and fastloop ported from
//! libdeflate's deflate_decompress.c and decompress_template.h. gzip/zlib
//! wrapper handling and error types are Rust-specific.

mod scan;
#[cfg(feature = "alloc")]
pub mod streaming;
pub use scan::{ScanError, ScanOutcome, deflate_scan, zlib_scan};

use crate::checksum;
use crate::error::DecompressionError;

// ---------------------------------------------------------------------------
// Decode table constants
// ---------------------------------------------------------------------------

pub(crate) const PRECODE_TABLEBITS: u32 = 7;
const PRECODE_ENOUGH: usize = 128;
// 12 bits (fdeflate's choice) measured no faster with double literals on
// zenpng's 106 PNG inputs (median 1.045 vs 1.036 of fdeflate's time) and
// slower on small images, where the bigger table costs more to build.
pub(crate) const LITLEN_TABLEBITS: u32 = 11;
const LITLEN_ENOUGH: usize = 2342;
pub(crate) const OFFSET_TABLEBITS: u32 = 8;
const OFFSET_ENOUGH: usize = 402;

// Decode table entry flags
pub(crate) const HUFFDEC_LITERAL: u32 = 0x8000_0000;
pub(crate) const HUFFDEC_EXCEPTIONAL: u32 = 0x0000_8000;
pub(crate) const HUFFDEC_SUBTABLE_POINTER: u32 = 0x0000_4000;
pub(crate) const HUFFDEC_END_OF_BLOCK: u32 = 0x0000_2000;
/// Litlen entry holding two literals: the first in bits 16-23, the second in
/// bits 8-15, the combined codeword length in bits 0-7. Always set together
/// with `HUFFDEC_LITERAL`; bits 8-15 then overlap the exceptional flags, so
/// test `HUFFDEC_LITERAL` before any of them.
pub(crate) const HUFFDEC_DOUBLE_LITERAL: u32 = 0x4000_0000;

// Bitstream constants (64-bit)
pub(crate) const CONSUMABLE_NBITS: u32 = 56; // MAX_BITSLEFT(63) - 7

// Fastloop safety margins — how many bytes the fastloop can read/write per iteration.
// Max bytes one fastloop iteration can write from its starting position: up to
// two litlen entries (two bytes each, one possibly scratch for a single
// literal), then a match whose chunked copy (32-byte chunks) can run up to
// 31 bytes past its end. Bytes past the decoded output may be scribbled (within this
// margin) and are overwritten by later output or left beyond `output_written`.
pub(crate) const FASTLOOP_MAX_BYTES_WRITTEN: usize =
    4 + crate::constants::DEFLATE_MAX_MATCH_LEN as usize + 32;
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

/// Result of a decompression operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DecompressOutcome {
    /// How many bytes of the input slice were consumed by the DEFLATE stream.
    pub input_consumed: usize,
    /// How many decompressed bytes were written to the output buffer.
    pub output_written: usize,
}

/// What the zlib and gzip decoders do with the stream's checksum (zlib's
/// Adler-32, gzip's CRC-32). Set with
/// [`Decompressor::with_checksum`]. Streaming decoders accept the same policy.
/// Raw DEFLATE has no checksum, so the policy doesn't apply to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChecksumPolicy {
    /// Compute the checksum and compare it with the stream's; a mismatch is
    /// [`DecompressionError::ChecksumMismatch`]. The default.
    #[default]
    Verify,
    /// Compute and compare, but report a mismatch through
    /// `checksum_matched()` (`Some(false)`) instead of failing. The output
    /// is still returned.
    Report,
    /// Neither compute nor compare: skips the checksum pass over the output.
    /// `checksum_matched()` returns `None`. The trailer is still read, and
    /// gzip's length field is still checked: a wrong length is
    /// [`DecompressionError::ChecksumMismatch`].
    Ignore,
}

/// DEFLATE/zlib/gzip decompressor.
///
/// Reusable across multiple decompression calls. Caches static Huffman
/// decode tables between calls for efficiency.
///
/// ```
/// use zenflate::{Compressor, CompressionLevel, Decompressor, Unstoppable};
///
/// // Compress some data
/// let data = b"The quick brown fox jumps over the lazy dog.";
/// let mut c = Compressor::new(CompressionLevel::fastest());
/// let bound = Compressor::deflate_compress_bound(data.len());
/// let mut compressed = vec![0u8; bound];
/// let csize = c.deflate_compress(data, &mut compressed, Unstoppable).unwrap();
///
/// // Decompress it back
/// let mut d = Decompressor::new();
/// let mut output = vec![0u8; data.len()];
/// let result = d.deflate_decompress(&compressed[..csize], &mut output, Unstoppable).unwrap();
/// assert_eq!(&output[..result.output_written], &data[..]);
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
    /// The litlen table holds double-literal entries (see `add_double_literals`).
    pub(crate) litlen_doubles: bool,
    checksum: ChecksumPolicy,
    checksum_matched: Option<bool>,
    max_output_size: Option<usize>,
}

impl core::fmt::Debug for Decompressor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decompressor")
            .field("static_codes_loaded", &self.static_codes_loaded)
            .field("checksum", &self.checksum)
            .field("checksum_matched", &self.checksum_matched)
            .field("max_output_size", &self.max_output_size)
            .finish_non_exhaustive()
    }
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
            litlen_doubles: false,
            checksum: ChecksumPolicy::Verify,
            checksum_matched: None,
            max_output_size: None,
        }
    }

    /// Clear per-stream decode state so the tables can serve a new stream
    /// (streaming `reset`). The tables themselves are not cleared: every
    /// block builds its tables before reading them.
    #[cfg(feature = "alloc")]
    pub(crate) fn reset_stream_state(&mut self) {
        self.static_codes_loaded = false;
        self.litlen_doubles = false;
        self.checksum_matched = None;
    }

    /// Load the fixed Huffman tables (RFC 1951 section 3.2.6). With `std`
    /// they are built once per process and copied: building them cost about
    /// 16K instructions per stream that starts with a fixed-Huffman block.
    pub(crate) fn load_static_tables(&mut self) -> bool {
        #[cfg(feature = "std")]
        {
            struct StaticTables {
                litlen: [u32; LITLEN_ENOUGH],
                offset: [u32; OFFSET_ENOUGH],
                litlen_tablebits: u32,
            }
            static CACHE: std::sync::OnceLock<Option<Box<StaticTables>>> =
                std::sync::OnceLock::new();
            let cached = CACHE.get_or_init(|| {
                let mut d = Box::new(Decompressor::new());
                d.build_static_tables().then(|| {
                    Box::new(StaticTables {
                        litlen: d.litlen_decode_table,
                        offset: d.offset_decode_table,
                        litlen_tablebits: d.litlen_tablebits,
                    })
                })
            });
            if let Some(t) = cached {
                self.litlen_decode_table = t.litlen;
                self.offset_decode_table = t.offset;
                self.litlen_tablebits = t.litlen_tablebits;
                return true;
            }
        }
        self.build_static_tables()
    }

    fn build_static_tables(&mut self) -> bool {
        self.lens[..144].fill(8);
        self.lens[144..256].fill(9);
        self.lens[256..280].fill(7);
        self.lens[280..288].fill(8);
        // Fixed offset code: all 5 bits
        self.lens[288..320].fill(5);
        build_decode_table(
            &mut self.offset_decode_table,
            &self.lens[288..],
            32,
            &OFFSET_DECODE_RESULTS,
            OFFSET_TABLEBITS,
            15,
            &mut self.sorted_syms,
            None,
        ) && build_decode_table(
            &mut self.litlen_decode_table,
            &self.lens,
            288,
            &LITLEN_DECODE_RESULTS,
            LITLEN_TABLEBITS,
            15,
            &mut self.sorted_syms,
            Some(&mut self.litlen_tablebits),
        )
    }

    /// Set a maximum output size limit for decompression.
    ///
    /// When set, decompression will return
    /// [`OutputLimitExceeded`](DecompressionError::OutputLimitExceeded) if the
    /// decompressed data would exceed this many bytes. This defends against
    /// decompression bombs — small compressed inputs that expand to enormous
    /// output.
    ///
    /// `None` (the default) means unlimited — output is bounded only by the
    /// caller-provided buffer size.
    ///
    /// # Example
    ///
    /// ```
    /// use zenflate::Decompressor;
    ///
    /// let d = Decompressor::new().with_max_output_size(Some(1024 * 1024)); // 1 MiB limit
    /// ```
    #[must_use]
    pub fn with_max_output_size(mut self, max: Option<usize>) -> Self {
        self.max_output_size = max;
        self
    }

    /// What to do with the zlib/gzip checksum (see [`ChecksumPolicy`]).
    /// Default: [`ChecksumPolicy::Verify`].
    #[must_use]
    pub fn with_checksum(mut self, policy: ChecksumPolicy) -> Self {
        self.checksum = policy;
        self
    }

    /// `true` is [`with_checksum(ChecksumPolicy::Report)`](Self::with_checksum),
    /// `false` is [`ChecksumPolicy::Verify`]. Prefer `with_checksum`.
    #[must_use]
    pub fn with_skip_checksum(self, skip: bool) -> Self {
        self.with_checksum(if skip {
            ChecksumPolicy::Report
        } else {
            ChecksumPolicy::Verify
        })
    }

    /// Whether the wrapper checksum matched after decompression.
    ///
    /// - `None` — footer not yet processed (raw DEFLATE or not yet
    ///   decompressed), or the policy is [`ChecksumPolicy::Ignore`]
    /// - `Some(true)` — checksum matched
    /// - `Some(false)` — checksum mismatch (also recorded before a
    ///   [`ChecksumPolicy::Verify`] error)
    #[must_use]
    pub fn checksum_matched(&self) -> Option<bool> {
        self.checksum_matched
    }

    /// Decompress raw DEFLATE data.
    ///
    /// DEFLATE is self-terminating, so the input slice may extend past the
    /// compressed data. Use [`DecompressOutcome::input_consumed`] to find
    /// where the stream ended.
    ///
    /// The `stop` parameter enables cooperative cancellation — checked at each
    /// block boundary (typically every 32–65 KB of output). Pass
    /// [`Unstoppable`](enough::Unstoppable) when cancellation is not needed;
    /// the compiler eliminates all checks.
    ///
    /// Bytes of `output` past the returned `output_written` may be
    /// overwritten (the fast decode loop stores in chunks of up to 32 bytes within a
    /// margin below `output.len()`); don't keep data there.
    pub fn deflate_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<DecompressOutcome, DecompressionError> {
        self.checksum_matched = None;
        let (input_consumed, output_written) =
            self.deflate_decompress_core(input, output, &stop)?;
        Ok(DecompressOutcome {
            input_consumed,
            output_written,
        })
    }

    /// Decompress zlib-wrapped data.
    ///
    /// See [`deflate_decompress`](Self::deflate_decompress) for the `stop`
    /// parameter.
    ///
    /// Bytes of `output` past the returned `output_written` may be
    /// overwritten (the fast decode loop stores in chunks of up to 32 bytes within a
    /// margin below `output.len()`); don't keep data there.
    pub fn zlib_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<DecompressOutcome, DecompressionError> {
        self.checksum_matched = None;
        let hdr_err = DecompressionError::InvalidHeader;

        if input.len() < ZLIB_MIN_OVERHEAD {
            return Err(hdr_err);
        }
        // 2-byte header (big-endian)
        let hdr = u16::from_be_bytes([input[0], input[1]]);
        if !hdr.is_multiple_of(31) {
            return Err(hdr_err);
        }
        if (input[0] & 0xF) != ZLIB_CM_DEFLATE {
            return Err(hdr_err);
        }
        if (input[0] >> 4) > ZLIB_CINFO_32K_WINDOW {
            return Err(hdr_err);
        }
        // FDICT not supported
        if (input[1] >> 5) & 1 != 0 {
            return Err(hdr_err);
        }

        let deflate_data = &input[2..input.len() - ZLIB_FOOTER_SIZE];
        let (deflate_consumed, output_written) =
            self.deflate_decompress_core(deflate_data, output, &stop)?;

        // Verify Adler-32 (big-endian, after DEFLATE data)
        let footer_start = 2 + deflate_consumed;
        let expected = u32::from_be_bytes([
            input[footer_start],
            input[footer_start + 1],
            input[footer_start + 2],
            input[footer_start + 3],
        ]);
        self.checksum_matched = None;
        if self.checksum != ChecksumPolicy::Ignore {
            let actual = checksum::adler32(1, &output[..output_written]);
            let matched = actual == expected;
            self.checksum_matched = Some(matched);
            if !matched && self.checksum == ChecksumPolicy::Verify {
                return Err(DecompressionError::ChecksumMismatch);
            }
        }

        Ok(DecompressOutcome {
            input_consumed: footer_start + ZLIB_FOOTER_SIZE,
            output_written,
        })
    }

    /// Decompress gzip-wrapped data.
    ///
    /// See [`deflate_decompress`](Self::deflate_decompress) for the `stop`
    /// parameter.
    ///
    /// Bytes of `output` past the returned `output_written` may be
    /// overwritten (the fast decode loop stores in chunks of up to 32 bytes within a
    /// margin below `output.len()`); don't keep data there.
    pub fn gzip_decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<DecompressOutcome, DecompressionError> {
        self.checksum_matched = None;
        let hdr_err = DecompressionError::InvalidHeader;

        if input.len() < GZIP_MIN_OVERHEAD {
            return Err(hdr_err);
        }
        let mut pos = 0;
        if input[pos] != GZIP_ID1 || input[pos + 1] != GZIP_ID2 {
            return Err(hdr_err);
        }
        pos += 2;
        if input[pos] != GZIP_CM_DEFLATE {
            return Err(hdr_err);
        }
        pos += 1;
        let flg = input[pos];
        pos += 1;
        // MTIME(4) + XFL(1) + OS(1) = 6 bytes
        pos += 6;

        if flg & GZIP_FRESERVED != 0 {
            return Err(hdr_err);
        }

        // Extra field
        if flg & GZIP_FEXTRA != 0 {
            if pos + 2 > input.len() {
                return Err(hdr_err);
            }
            let xlen = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
            pos += 2;
            if input.len() - pos < xlen + GZIP_FOOTER_SIZE {
                return Err(hdr_err);
            }
            pos += xlen;
        }

        // Original file name (zero terminated)
        if flg & GZIP_FNAME != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            // Must have found a null terminator, not run off the end
            if pos >= input.len() {
                return Err(hdr_err);
            }
            pos += 1; // skip the null terminator
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(hdr_err);
            }
        }

        // File comment (zero terminated)
        if flg & GZIP_FCOMMENT != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            if pos >= input.len() {
                return Err(hdr_err);
            }
            pos += 1;
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(hdr_err);
            }
        }

        // CRC16 for gzip header
        if flg & GZIP_FHCRC != 0 {
            pos += 2;
            if input.len() - pos < GZIP_FOOTER_SIZE {
                return Err(hdr_err);
            }
        }

        // Compressed DEFLATE data
        let deflate_end = input.len() - GZIP_FOOTER_SIZE;
        if pos > deflate_end {
            return Err(hdr_err);
        }
        let (deflate_consumed, output_written) =
            self.deflate_decompress_core(&input[pos..deflate_end], output, &stop)?;

        let footer_start = pos + deflate_consumed;

        // CRC32 (little-endian)
        let expected_crc = u32::from_le_bytes([
            input[footer_start],
            input[footer_start + 1],
            input[footer_start + 2],
            input[footer_start + 3],
        ]);
        let ignore = self.checksum == ChecksumPolicy::Ignore;
        let crc_ok = ignore || checksum::crc32(0, &output[..output_written]) == expected_crc;

        // ISIZE (little-endian, mod 2^32)
        let expected_size = u32::from_le_bytes([
            input[footer_start + 4],
            input[footer_start + 5],
            input[footer_start + 6],
            input[footer_start + 7],
        ]);
        let size_ok = (output_written as u32) == expected_size;

        let matched = crc_ok && size_ok;
        self.checksum_matched = if ignore { None } else { Some(matched) };
        if !matched && self.checksum != ChecksumPolicy::Report {
            return Err(DecompressionError::ChecksumMismatch);
        }

        Ok(DecompressOutcome {
            input_consumed: footer_start + GZIP_FOOTER_SIZE,
            output_written,
        })
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

/// Look up a decode table entry by index.
#[inline(always)]
pub(crate) fn table_lookup(table: &[u32], idx: u64) -> u32 {
    table[idx as usize]
}

/// Store a litlen entry's literal(s) and return how many: one store when the
/// table has no double entries (no dependency of `pos` on the entry), else
/// [`store_lits`]. Fastloop only.
#[inline(always)]
pub(crate) fn put_lits(output: &mut [u8], pos: usize, entry: u32, doubles: bool) -> usize {
    if doubles {
        store_lits(output, pos, entry)
    } else {
        output[pos] = (entry >> 16) as u8;
        1
    }
}

/// Store the one or two literals of a litlen entry and return how many.
///
/// Always writes two bytes (the second is scratch for a single literal), so
/// `pos + 1` must be in bounds: fastloop only.
#[inline(always)]
pub(crate) fn store_lits(output: &mut [u8], pos: usize, entry: u32) -> usize {
    output[pos] = (entry >> 16) as u8;
    output[pos + 1] = (entry >> 8) as u8;
    1 + ((entry >> 30) & 1) as usize
}

/// Build double-literal entries only when at least this much compressed input
/// is left: the pass touches every main-table entry, which costs more than it
/// saves on small streams (64x64 PNGs decoded 10% slower one-shot on
/// Neoverse-N1 with an unconditional pass; 256x256 and up were 3-25% faster).
///
/// Unit tests and fuzzing use 0 so double entries are exercised on every
/// stream; integration tests run the real threshold.
#[cfg(not(any(test, fuzzing)))]
pub(crate) const DOUBLE_LITERAL_MIN_INPUT: usize = 16 * 1024;
#[cfg(any(test, fuzzing))]
pub(crate) const DOUBLE_LITERAL_MIN_INPUT: usize = 0;

/// Whether a table built with `remaining` compressed bytes left should get
/// double-literal entries.
#[inline]
#[allow(clippy::absurd_extreme_comparisons)] // the threshold is 0 under test/fuzzing
pub(crate) fn wants_double_literals(remaining: usize) -> bool {
    remaining >= DOUBLE_LITERAL_MIN_INPUT
}

/// Turn primary litlen entries whose codeword is followed, within
/// `table_bits`, by a second literal's whole codeword into double-literal
/// entries (see [`HUFFDEC_DOUBLE_LITERAL`]).
///
/// Filtered PNG rows are mostly literals, so this halves the table lookups
/// there (the trick image-rs's fdeflate uses). Entries are visited from the
/// top down: `i >> len1 < i` for every `i > 0`, so the entry read for the
/// second literal is still its single-literal form.
pub(crate) fn add_double_literals(table: &mut [u32], table_bits: u32) {
    let size = 1usize << table_bits;
    for i in (0..size).rev() {
        let e1 = table[i];
        if e1 & HUFFDEC_LITERAL == 0 {
            continue;
        }
        let len1 = e1 & 0xFF;
        if len1 >= table_bits {
            continue;
        }
        let e2 = table[i >> len1];
        if e2 & (HUFFDEC_LITERAL | HUFFDEC_DOUBLE_LITERAL) != HUFFDEC_LITERAL {
            continue;
        }
        let len2 = e2 & 0xFF;
        if len1 + len2 > table_bits {
            continue;
        }
        table[i] = HUFFDEC_LITERAL
            | HUFFDEC_DOUBLE_LITERAL
            | (e1 & 0x00FF_0000)
            | ((e2 >> 8) & 0xFF00)
            | (len1 + len2);
    }
}

/// Fastloop match copy in fixed-size chunks (each compiles to vector loads and
/// stores, no `memmove` call), as image-rs's fdeflate does with 16-byte
/// chunks. Requires `out_pos + length + 31 <= output.len()` (the fastloop
/// margin).
///
/// - offset >= 32: 32-byte chunks; they don't overlap their source. (Measured
///   faster than 16-byte chunks on Core Ultra 7 265K, Ryzen 7950X and
///   Neoverse-N1, `examples/png_inflate.rs`.)
/// - offset 16..=31: 16-byte chunks; they don't overlap their source.
/// - offset 1: a run; 16-byte splats of the byte.
/// - offset 2..=15: 16-byte copies stepping by `offset`: each chunk's first
///   `offset` bytes come from output that is already final, and the rest are
///   overwritten by the next chunk.
#[inline(always)]
pub(crate) fn fastloop_match_copy(
    output: &mut [u8],
    out_pos: usize,
    src_start: usize,
    length: usize,
    offset: usize,
) {
    if offset >= 32 {
        let mut i = 0;
        loop {
            output.copy_within(src_start + i..src_start + i + 32, out_pos + i);
            i += 32;
            if i >= length {
                break;
            }
        }
    } else if offset >= 16 {
        let mut i = 0;
        loop {
            output.copy_within(src_start + i..src_start + i + 16, out_pos + i);
            i += 16;
            if i >= length {
                break;
            }
        }
    } else if offset == 1 {
        let splat = [output[src_start]; 16];
        let mut i = 0;
        loop {
            output[out_pos + i..out_pos + i + 16].copy_from_slice(&splat);
            i += 16;
            if i >= length {
                break;
            }
        }
    } else {
        let mut i = 0;
        loop {
            output.copy_within(src_start + i..src_start + i + 16, out_pos + i);
            i += offset;
            if i >= length {
                break;
            }
        }
    }
}

#[cfg(test)]
#[path = "match_copy_tests.rs"]
mod match_copy_tests;

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
        stop: &impl enough::Stop,
    ) -> Result<(usize, usize), DecompressionError> {
        // x86-64-v4 (AVX-512) build of this loop for inputs of 16 KiB and up.
        // Time vs fdeflate on zenpng's 106 PNG streams, Ryzen 9 9950X3D (Zen
        // 5): 1.007 -> 0.976 (256 px and up: 1.00 -> 0.96), Silesia +
        // Canterbury 0.901 -> 0.880. Ungated, 64 px images (under 8 KB of
        // input) were 3.6% slower. The 7950X (Zen 4) gained 1-2%.
        #[cfg(all(feature = "avx512", target_arch = "x86_64"))]
        {
            use archmage::SimdToken;
            if input.len() >= ONESHOT_V4_MIN_INPUT
                && let Some(token) = archmage::X64V4Token::summon()
            {
                return oneshot_v4::core_v4(token, self, input, output, stop);
            }
        }
        self.deflate_decompress_core_impl::<false>(input, output, usize::MAX, &mut 0, stop)
            .map(|(i, o, _)| (i, o))
    }

    /// The inflate loop. `COUNT` is the count-only scan ([`deflate_scan`]): nothing is written
    /// (`output` is unused), double-literal entries are off so each symbol's bits are
    /// known, the loop returns `(consumed, out, true)` once `stop_at` output bytes exist,
    /// and a data error records the input position in `fail_at`. The decode instantiation
    /// (`COUNT = false`, `stop_at = usize::MAX`) compiles to the plain loop.
    #[inline(always)]
    pub(crate) fn deflate_decompress_core_impl<const COUNT: bool>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        stop_at: usize,
        fail_at: &mut usize,
        stop: &impl enough::Stop,
    ) -> Result<(usize, usize, bool), DecompressionError> {
        // No x86-64-v3 build of this loop (unlike the streaming decoder):
        // measured 1.8-2.5% slower on Core Ultra 7 265K, while streaming
        // gained 3.5%. v4 is above.
        let mut in_pos: usize = 0;
        let mut out_pos: usize = 0;
        let mut bitbuf: u64 = 0;
        let mut bitsleft: u32 = 0;
        let mut overread_count: usize = 0;

        let bad = DecompressionError::BadData;
        macro_rules! fail {
            () => {{
                if COUNT {
                    *fail_at = in_pos;
                }
                return Err(bad);
            }};
        }
        // Count mode: `stop_at` output bytes exist; report the input through the last
        // whole byte holding bits of the symbol that produced them.
        macro_rules! stopped {
            () => {{
                let held = (bitsleft / 8) as usize;
                if overread_count > held {
                    fail!();
                }
                return Ok((in_pos + overread_count - held, out_pos, true));
            }};
        }

        // When max_output_size is set and is smaller than the output buffer,
        // use OutputLimitExceeded instead of InsufficientSpace. The effective
        // output limit is the smaller of the buffer and the policy limit.
        let (out_limit, no_space) = match self.max_output_size {
            _ if COUNT => (usize::MAX, DecompressionError::OutputLimitExceeded),
            Some(max) if max < output.len() => (max, DecompressionError::OutputLimitExceeded),
            _ => (output.len(), DecompressionError::InsufficientSpace),
        };

        loop {
            // Cooperative cancellation check at each block boundary.
            // With Unstoppable, this compiles to nothing.
            stop.check()?;

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
                    fail!();
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
                            fail!();
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
                    fail!();
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
                    fail!();
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
                    fail!();
                }
                self.litlen_doubles = !COUNT && wants_double_literals(input.len() - in_pos);
                if self.litlen_doubles {
                    add_double_literals(&mut self.litlen_decode_table, self.litlen_tablebits);
                }
            } else if block_type == DEFLATE_BLOCKTYPE_UNCOMPRESSED {
                // --- Uncompressed block ---
                bitsleft -= 3;

                // Align to byte boundary: rewind input past unconsumed bytes
                let extra_bytes = (bitsleft / 8) as usize;
                if overread_count > extra_bytes {
                    fail!();
                }
                in_pos -= extra_bytes - overread_count;
                overread_count = 0;
                bitbuf = 0;
                bitsleft = 0;

                // Read LEN and NLEN
                if in_pos + 4 > input.len() {
                    fail!();
                }
                let len = u16::from_le_bytes([input[in_pos], input[in_pos + 1]]) as usize;
                let nlen = u16::from_le_bytes([input[in_pos + 2], input[in_pos + 3]]);
                in_pos += 4;

                if len != (!nlen) as usize {
                    fail!();
                }
                if len > out_limit - out_pos {
                    return Err(no_space);
                }
                if COUNT && len >= stop_at - out_pos && stop_at - out_pos <= input.len() - in_pos {
                    return Ok((in_pos + (stop_at - out_pos), stop_at, true));
                }
                if len > input.len() - in_pos {
                    fail!();
                }

                if !COUNT {
                    output[out_pos..out_pos + len].copy_from_slice(&input[in_pos..in_pos + len]);
                }
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

                    if !self.load_static_tables() {
                        fail!();
                    }
                    self.litlen_doubles = !COUNT && wants_double_literals(input.len() - in_pos);
                    if self.litlen_doubles {
                        add_double_literals(&mut self.litlen_decode_table, self.litlen_tablebits);
                    }
                }
            } else {
                fail!();
            }

            // --- Fastloop + generic decode loop (literals and matches) ---
            let litlen_tablemask = bitmask(self.litlen_tablebits);
            let doubles = self.litlen_doubles;
            let in_fastloop_end = input.len().saturating_sub(FASTLOOP_MAX_BYTES_READ);
            let out_fastloop_end =
                if COUNT { stop_at } else { out_limit }.saturating_sub(FASTLOOP_MAX_BYTES_WRITTEN);

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

                    // --- Fast literal path: up to 3 entries of 1-2 literals ---
                    if entry & HUFFDEC_LITERAL != 0 {
                        // 1st entry (the primary item)
                        let lits = entry;
                        entry = table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                        saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;
                        out_pos += if COUNT {
                            1
                        } else {
                            put_lits(output, out_pos, lits, doubles)
                        };

                        if entry & HUFFDEC_LITERAL != 0 {
                            // 2nd entry
                            let lits = entry;
                            entry =
                                table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);
                            saved_bitbuf = bitbuf;
                            bitbuf >>= (entry & 0xFF) as u64;
                            bitsleft -= entry & 0xFF;
                            out_pos += if COUNT {
                                1
                            } else {
                                put_lits(output, out_pos, lits, doubles)
                            };

                            if entry & HUFFDEC_LITERAL != 0 {
                                // 3rd entry (replaces primary for next iter)
                                out_pos += if COUNT {
                                    1
                                } else {
                                    put_lits(output, out_pos, entry, doubles)
                                };
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
                            // Literal from subtable (never a double)
                            if !COUNT {
                                output[out_pos] = (entry >> 16) as u8;
                            }
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

                    // Conditional refill: after a multi-literal chain +
                    // length decode, bitsleft may be too low to consume the
                    // full offset entry (up to 28 bits) and still preload
                    // the next litlen entry. Mirror libdeflate's conditional
                    // REFILL_BITS_IN_FASTLOOP between offset preload and
                    // offset consumption on 64-bit.
                    if bitsleft < 28 + self.litlen_tablebits {
                        refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                    }

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
                        fail!();
                    }

                    // Refill BEFORE preload: after a multi-literal + match path,
                    // bitsleft can be < litlen_tablebits, causing the preload to
                    // read stale zero bits. Refilling first ensures enough valid
                    // bits for the table lookup.
                    refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                    entry = table_lookup(&self.litlen_decode_table, bitbuf & litlen_tablemask);

                    // Copy match data
                    if !COUNT {
                        fastloop_match_copy(output, out_pos, out_pos - offset, length, offset);
                    }
                    out_pos += length;

                    if in_pos >= in_fastloop_end || out_pos >= out_fastloop_end {
                        break 'fastloop;
                    }
                }
            }

            // --- Generic decode loop (handles remainder after fastloop) ---
            if !block_done {
                stop.check()?;
                // Periodic stop check interval (output bytes). 16KB at 600+ MiB/s ≈ <0.03ms.
                const DECOMPRESS_STOP_INTERVAL: usize = 16384;
                // (count mode on 32-bit targets can approach usize::MAX)
                let next = |p: usize| {
                    if COUNT {
                        p.saturating_add(DECOMPRESS_STOP_INTERVAL)
                    } else {
                        p + DECOMPRESS_STOP_INTERVAL
                    }
                };
                let mut next_stop_check = next(out_pos);
                loop {
                    if out_pos >= next_stop_check {
                        stop.check()?;
                        next_stop_check = next(out_pos);
                    }
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

                    // Resolve subtable if needed (a double literal's second
                    // byte overlaps the flag bits, so rule out literals first)
                    if entry & (HUFFDEC_LITERAL | HUFFDEC_SUBTABLE_POINTER)
                        == HUFFDEC_SUBTABLE_POINTER
                    {
                        entry = table_lookup(
                            &self.litlen_decode_table,
                            (entry >> 16) as u64 + extract_varbits(bitbuf, (entry >> 8) & 0x3F),
                        );
                        saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;
                    }

                    let value = entry >> 16;

                    // Literal (one, or two for a double entry)?
                    if entry & HUFFDEC_LITERAL != 0 {
                        if out_pos >= out_limit {
                            return Err(no_space);
                        }
                        if !COUNT {
                            output[out_pos] = value as u8;
                        }
                        out_pos += 1;
                        if entry & HUFFDEC_DOUBLE_LITERAL != 0 {
                            if out_pos >= out_limit {
                                return Err(no_space);
                            }
                            if !COUNT {
                                output[out_pos] = (entry >> 8) as u8;
                            }
                            out_pos += 1;
                        }
                        if COUNT && out_pos >= stop_at {
                            stopped!();
                        }
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

                    if length > out_limit - out_pos {
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
                        fail!();
                    }

                    // Copy match data (may overlap when offset < length)
                    let src_start = out_pos - offset;
                    if COUNT {
                        out_pos += length;
                        if out_pos >= stop_at {
                            stopped!();
                        }
                        continue;
                    }
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
            fail!();
        }

        // Compute actual input consumed
        let actual_in = in_pos - ((final_bitsleft / 8) as usize - overread_count);

        Ok((actual_in, out_pos, false))
    }
}

// All decompress tests use libdeflater (C FFI) to create test data.
#[cfg(all(test, not(miri), not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// The process-wide fixed-Huffman tables (std) equal freshly built ones,
    /// and a second load (served from the cache) does too.
    #[test]
    fn static_tables_cache_matches_build() {
        let mut built = Decompressor::new();
        assert!(built.build_static_tables());
        for _ in 0..2 {
            let mut loaded = Decompressor::new();
            assert!(loaded.load_static_tables());
            assert_eq!(loaded.litlen_decode_table, built.litlen_decode_table);
            assert_eq!(loaded.offset_decode_table, built.offset_decode_table);
            assert_eq!(loaded.litlen_tablebits, built.litlen_tablebits);
        }
    }

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
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
                .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap()
                .output_written;
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
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap()
            .output_written;
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
                .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap()
                .output_written;
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
                .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap()
                .output_written;
            assert_eq!(out_size, data.len(), "deflate level {level}");
            assert_eq!(output, data, "deflate level {level}");

            // zlib
            let bound = c.zlib_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.zlib_compress(&data, &mut compressed).unwrap();
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap()
                .output_written;
            assert_eq!(out_size, data.len(), "zlib level {level}");
            assert_eq!(output, data, "zlib level {level}");

            // gzip
            let bound = c.gzip_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.gzip_compress(&data, &mut compressed).unwrap();
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];
            let out_size = d
                .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap()
                .output_written;
            assert_eq!(out_size, data.len(), "gzip level {level}");
            assert_eq!(output, data, "gzip level {level}");
        }
    }

    // -----------------------------------------------------------------------
    // Edge case / robustness tests (inspired by flate2 issues #258, #474, #499)
    // -----------------------------------------------------------------------

    /// flate2 #499: short garbage input silently accepted.
    /// Verify we reject invalid inputs of all short lengths.
    #[test]
    fn reject_short_garbage_deflate() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];
        for len in 0..=16 {
            let garbage: Vec<u8> = (0..len).collect();
            // Most short inputs are not valid DEFLATE streams
            // (some 1-2 byte inputs might be valid empty blocks, but random bytes shouldn't be)
            let _ = d.deflate_decompress(&garbage, &mut output, enough::Unstoppable);
            // No panic = success. We don't assert error because a few short
            // byte sequences are technically valid DEFLATE (e.g., 0x03 0x00 is
            // a valid empty static Huffman block).
        }
    }

    /// flate2 #258/#499: invalid zlib data silently accepted.
    /// Single-byte and short garbage must return InvalidHeader.
    #[test]
    fn reject_short_garbage_zlib() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];
        // Anything shorter than ZLIB_MIN_OVERHEAD (6) must be rejected
        for len in 0..6 {
            let garbage: Vec<u8> = (0..len).map(|i| i as u8 + 77).collect();
            let err = d
                .zlib_decompress(&garbage, &mut output, enough::Unstoppable)
                .unwrap_err();
            assert_eq!(
                err,
                DecompressionError::InvalidHeader,
                "zlib should reject {len}-byte garbage"
            );
        }
        // Specific flate2 #258 reproduction: single byte [77]
        let err = d
            .zlib_decompress(&[77], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// flate2 #499: short garbage gzip input.
    /// Anything shorter than GZIP_MIN_OVERHEAD (18) must be rejected.
    #[test]
    fn reject_short_garbage_gzip() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];
        for len in 0..18 {
            let garbage: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let err = d
                .gzip_decompress(&garbage, &mut output, enough::Unstoppable)
                .unwrap_err();
            assert_eq!(
                err,
                DecompressionError::InvalidHeader,
                "gzip should reject {len}-byte garbage"
            );
        }
    }

    /// Verify that zlib correctly rejects various invalid headers.
    #[test]
    fn reject_invalid_zlib_headers() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];

        // Bad compression method (not 8)
        let err = d
            .zlib_decompress(&[0x19, 0x01, 0, 0, 0, 0], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);

        // Bad CINFO (window > 32K)
        let err = d
            .zlib_decompress(&[0x88, 0x01, 0, 0, 0, 0], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);

        // Bad checksum (CMF*256 + FLG not multiple of 31)
        let err = d
            .zlib_decompress(&[0x78, 0x00, 0, 0, 0, 0], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);

        // FDICT set (not supported)
        // 0x78 0xBB: CM=8, CINFO=7, FDICT=1, but checksum must be valid
        // Let's find a valid FDICT header: CMF=0x78, FLG must have bit5=1
        // and (0x78 << 8 | FLG) % 31 == 0. 0x78 << 8 = 0x7800.
        // 0x7800 + FLG ≡ 0 (mod 31). 0x7800 % 31 = 30720 % 31 = 30720 - 991*31 = 30720-30721 = need to recalc
        // Just set FDICT bit and fix checksum: 0x78 0xBB = 0x78BB, 0x78BB % 31 = 30907 % 31 = 30907 - 997*31 = 30907-30907 = 0. Valid!
        let err = d
            .zlib_decompress(&[0x78, 0xBB, 0, 0, 0, 0], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// Verify gzip rejects invalid magic bytes, bad CM, and reserved flags.
    #[test]
    fn reject_invalid_gzip_headers() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];

        // Wrong magic bytes
        let mut bad_magic = vec![0u8; 20];
        bad_magic[0] = 0x1F;
        bad_magic[1] = 0x00; // wrong ID2
        let err = d
            .gzip_decompress(&bad_magic, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);

        // Wrong compression method
        let mut bad_cm = vec![0u8; 20];
        bad_cm[0] = GZIP_ID1;
        bad_cm[1] = GZIP_ID2;
        bad_cm[2] = 9; // not DEFLATE
        let err = d
            .gzip_decompress(&bad_cm, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);

        // Reserved flag bits set
        let mut reserved = vec![0u8; 20];
        reserved[0] = GZIP_ID1;
        reserved[1] = GZIP_ID2;
        reserved[2] = GZIP_CM_DEFLATE;
        reserved[3] = 0xE0; // reserved bits
        let err = d
            .gzip_decompress(&reserved, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// Regression test for FNAME without null terminator.
    /// Before the fix, this caused a usize underflow (debug panic, release UB).
    #[test]
    fn reject_gzip_fname_no_null_terminator() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];

        // Valid gzip header with FNAME flag, but the name field has no null terminator
        let mut input = vec![0u8; 30];
        input[0] = GZIP_ID1;
        input[1] = GZIP_ID2;
        input[2] = GZIP_CM_DEFLATE;
        input[3] = GZIP_FNAME; // FNAME flag
        // bytes 4-9: MTIME + XFL + OS (zeros)
        // bytes 10+: "filename" with no null terminator
        input[10..30].fill(b'A');

        let err = d
            .gzip_decompress(&input, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// Regression test for FCOMMENT without null terminator.
    #[test]
    fn reject_gzip_fcomment_no_null_terminator() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];

        let mut input = vec![0u8; 30];
        input[0] = GZIP_ID1;
        input[1] = GZIP_ID2;
        input[2] = GZIP_CM_DEFLATE;
        input[3] = GZIP_FCOMMENT;
        input[10..30].fill(b'C');

        let err = d
            .gzip_decompress(&input, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// Regression test for FNAME + FCOMMENT both without null terminators.
    #[test]
    fn reject_gzip_fname_and_fcomment_no_null() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];

        let mut input = vec![0u8; 30];
        input[0] = GZIP_ID1;
        input[1] = GZIP_ID2;
        input[2] = GZIP_CM_DEFLATE;
        input[3] = GZIP_FNAME | GZIP_FCOMMENT;
        input[10..30].fill(b'X');

        let err = d
            .gzip_decompress(&input, &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    // -----------------------------------------------------------------------
    // skip_checksum tests
    // -----------------------------------------------------------------------

    /// Corrupt Adler-32 in valid zlib: strict mode fails, skip mode succeeds
    /// and reports checksum_matched() == Some(false).
    #[test]
    fn zlib_skip_checksum_corrupt_adler() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(&data, &mut compressed).unwrap();

        // Corrupt the last byte of the Adler-32 footer
        compressed[csize - 1] ^= 0xFF;

        // Strict mode: should fail
        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let err = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::ChecksumMismatch);

        // Skip mode: should succeed with checksum_matched = Some(false)
        let mut d = Decompressor::new().with_skip_checksum(true);
        let mut output = vec![0u8; data.len()];
        let result = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, data.len());
        assert_eq!(&output[..result.output_written], &data[..]);
        assert_eq!(d.checksum_matched(), Some(false));
    }

    /// Corrupt CRC32 in valid gzip: strict mode fails, skip mode succeeds.
    #[test]
    fn gzip_skip_checksum_corrupt_crc() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(&data, &mut compressed).unwrap();

        // Corrupt CRC32 (4 bytes before ISIZE, which is 4 bytes before end)
        compressed[csize - 8] ^= 0xFF;

        // Strict mode: should fail
        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let err = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::ChecksumMismatch);

        // Skip mode: should succeed
        let mut d = Decompressor::new().with_skip_checksum(true);
        let mut output = vec![0u8; data.len()];
        let result = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, data.len());
        assert_eq!(&output[..result.output_written], &data[..]);
        assert_eq!(d.checksum_matched(), Some(false));
    }

    /// ChecksumPolicy on the one-shot decoders: Verify fails a corrupt
    /// checksum, Report records it, Ignore neither computes nor records it,
    /// and gzip's length field fails under every policy but Report.
    #[test]
    fn checksum_policy_one_shot() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let mut z = vec![0u8; c.zlib_compress_bound(data.len())];
        let zn = c.zlib_compress(&data, &mut z).unwrap();
        let mut g = vec![0u8; c.gzip_compress_bound(data.len())];
        let gn = c.gzip_compress(&data, &mut g).unwrap();
        let (mut z, mut g) = (z[..zn].to_vec(), g[..gn].to_vec());
        let mut out = vec![0u8; data.len()];
        let run = |d: &mut Decompressor, gz: bool, input: &[u8], out: &mut [u8]| {
            if gz {
                d.gzip_decompress(input, out, enough::Unstoppable)
            } else {
                d.zlib_decompress(input, out, enough::Unstoppable)
            }
        };
        for gz in [false, true] {
            let input = if gz { &mut g } else { &mut z };
            // Valid stream.
            for (policy, matched) in [
                (ChecksumPolicy::Verify, Some(true)),
                (ChecksumPolicy::Report, Some(true)),
                (ChecksumPolicy::Ignore, None),
            ] {
                let mut d = Decompressor::new().with_checksum(policy);
                run(&mut d, gz, input, &mut out).unwrap();
                assert_eq!(out, data);
                assert_eq!(d.checksum_matched(), matched, "{policy:?} gz={gz}");
            }
            // Corrupt checksum (CRC-32 for gzip, Adler-32 for zlib).
            let at = if gz { input.len() - 8 } else { input.len() - 1 };
            input[at] ^= 0xFF;
            let mut d = Decompressor::new().with_checksum(ChecksumPolicy::Verify);
            assert_eq!(
                run(&mut d, gz, input, &mut out).unwrap_err(),
                DecompressionError::ChecksumMismatch
            );
            let mut d = Decompressor::new().with_checksum(ChecksumPolicy::Report);
            run(&mut d, gz, input, &mut out).unwrap();
            assert_eq!(d.checksum_matched(), Some(false));
            // A reused decoder: Ignore clears the previous call's result.
            let mut d = d.with_checksum(ChecksumPolicy::Ignore);
            run(&mut d, gz, input, &mut out).unwrap();
            assert_eq!((d.checksum_matched(), &out), (None, &data));
            input[at] ^= 0xFF;
        }
        // gzip length field: Ignore still fails it, Report records it.
        let at = g.len() - 4;
        g[at] ^= 0x01;
        let mut d = Decompressor::new().with_checksum(ChecksumPolicy::Ignore);
        assert_eq!(
            run(&mut d, true, &g, &mut out).unwrap_err(),
            DecompressionError::ChecksumMismatch
        );
        let mut d = Decompressor::new().with_checksum(ChecksumPolicy::Report);
        run(&mut d, true, &g, &mut out).unwrap();
        assert_eq!(d.checksum_matched(), Some(false));
    }

    /// Valid zlib with skip_checksum: checksum_matched() == Some(true).
    #[test]
    fn zlib_skip_checksum_valid_reports_true() {
        let data = b"hello, skip_checksum test";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(data, &mut compressed).unwrap();

        let mut d = Decompressor::new().with_skip_checksum(true);
        let mut output = vec![0u8; data.len()];
        d.zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(d.checksum_matched(), Some(true));
    }

    /// Structural errors still fail even with skip_checksum.
    #[test]
    fn skip_checksum_does_not_skip_structural_errors() {
        // Invalid zlib header
        let mut d = Decompressor::new().with_skip_checksum(true);
        let mut output = vec![0u8; 1024];
        let err = d
            .zlib_decompress(&[0x00, 0x00, 0, 0, 0, 0], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::InvalidHeader);
    }

    /// checksum_matched is None for raw DEFLATE (no wrapper checksum).
    #[test]
    fn deflate_checksum_matched_is_none() {
        let data = b"raw deflate test";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut d = Decompressor::new().with_skip_checksum(true);
        let mut output = vec![0u8; data.len()];
        d.deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(d.checksum_matched(), None);
    }

    /// Broad garbage rejection: 256 different single-byte inputs to all formats.
    #[test]
    fn reject_single_byte_all_formats() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];
        for b in 0..=255u8 {
            // zlib: always InvalidHeader (min 6 bytes)
            let err = d
                .zlib_decompress(&[b], &mut output, enough::Unstoppable)
                .unwrap_err();
            assert_eq!(err, DecompressionError::InvalidHeader);

            // gzip: always InvalidHeader (min 18 bytes)
            let err = d
                .gzip_decompress(&[b], &mut output, enough::Unstoppable)
                .unwrap_err();
            assert_eq!(err, DecompressionError::InvalidHeader);

            // deflate: should not panic (some bytes may decode as valid tiny blocks)
            let _ = d.deflate_decompress(&[b], &mut output, enough::Unstoppable);
        }
    }

    // =====================================================================
    // Upstream bug pattern tests (libdeflate, flate2, miniz_oxide, zlib)
    // =====================================================================

    /// Empty stored block: BFINAL=1, BTYPE=00, LEN=0, NLEN=0xFFFF.
    /// Exercises stored block path with zero-length copy (libdeflate #157).
    #[test]
    fn empty_stored_block_final() {
        // BFINAL=1, BTYPE=00 → byte 0x01, then padding to byte boundary,
        // LEN=0x0000, NLEN=0xFFFF
        let data = [0x01, 0x00, 0x00, 0xff, 0xff];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 64];
        let result = d
            .deflate_decompress(&data, &mut output, enough::Unstoppable)
            .expect("empty stored block should decompress");
        assert_eq!(result.output_written, 0);
    }

    /// Non-final empty stored block followed by a final empty stored block.
    /// Two zero-length stored blocks in sequence.
    #[test]
    fn two_empty_stored_blocks() {
        // First block: BFINAL=0, BTYPE=00, LEN=0, NLEN=0xFFFF
        // Second block: BFINAL=1, BTYPE=00, LEN=0, NLEN=0xFFFF
        let data = [
            0x00, 0x00, 0x00, 0xff, 0xff, // non-final empty
            0x01, 0x00, 0x00, 0xff, 0xff, // final empty
        ];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 64];
        let result = d
            .deflate_decompress(&data, &mut output, enough::Unstoppable)
            .expect("two empty stored blocks should decompress");
        assert_eq!(result.output_written, 0);
    }

    /// Stored block with LEN != ~NLEN must be rejected.
    #[test]
    fn reject_stored_block_bad_nlen() {
        // BFINAL=1, BTYPE=00, LEN=0x0005, NLEN=0x0000 (wrong, should be 0xFFFA)
        let data = [0x01, 0x05, 0x00, 0x00, 0x00];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 64];
        assert!(
            d.deflate_decompress(&data, &mut output, enough::Unstoppable)
                .is_err()
        );
    }

    /// Block type 3 (reserved) must be rejected per RFC 1951.
    #[test]
    fn reject_reserved_block_type() {
        // BFINAL=1, BTYPE=11 (reserved) → bottom 3 bits = 0b111 = 0x07
        let data = [0x07, 0x00, 0x00, 0x00, 0x00];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 64];
        assert!(
            d.deflate_decompress(&data, &mut output, enough::Unstoppable)
                .is_err()
        );
    }

    /// Truncated dynamic Huffman header (too few bytes for code lengths).
    /// Must error, not panic from out-of-bounds (miniz_oxide #130).
    #[test]
    fn reject_truncated_dynamic_huffman() {
        // BFINAL=1, BTYPE=10 (dynamic) → bottom 3 bits = 0b101 = 0x05
        // Then just a few garbage bytes — not enough for the full header
        for len in 1..=6 {
            let mut data = vec![0x05u8; len];
            data[0] = 0x05; // only first byte matters
            let mut d = Decompressor::new();
            let mut output = vec![0u8; 64];
            // Should fail gracefully, not panic
            assert!(
                d.deflate_decompress(&data, &mut output, enough::Unstoppable)
                    .is_err()
            );
        }
    }

    /// All two-byte inputs to deflate: no panics.
    /// Catches boundary conditions in short dynamic/static block headers.
    #[test]
    fn two_byte_deflate_no_panic() {
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 1024];
        // Sample all 65536 two-byte combinations
        for hi in 0..=255u8 {
            for lo in 0..=255u8 {
                let _ = d.deflate_decompress(&[lo, hi], &mut output, enough::Unstoppable);
            }
        }
    }

    /// miniz_oxide #137: zlib stream with incomplete Huffman tree must be rejected.
    /// This is a valid zlib header wrapping a dynamic Huffman block whose
    /// literal/length code is not a complete prefix code.
    #[test]
    fn reject_incomplete_huffman_tree_miniz137() {
        let data: &[u8] = &[
            120, 1, 237, 224, 144, 1, 36, 73, 146, 36, 73, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 122, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 4096];
        // Must return an error (bad Huffman table), not panic or return garbage
        assert!(
            d.zlib_decompress(data, &mut output, enough::Unstoppable)
                .is_err()
        );
    }

    /// miniz_oxide #161: garbage input that triggered a panic in match copy.
    /// Must return Err, not panic from out-of-bounds.
    #[test]
    fn garbage_input_no_panic_miniz161() {
        let data: &[u8] = &[
            0xfa, 0x99, 0xff, 0xf4, 0xf3, 0x7f, 0xef, 0x5b, 0xbf, 0xf9, 0xbb, 0x6c, 0xcb, 0x9a,
            0xb4, 0xe4, 0x7f, 0x66, 0xd9, 0x87, 0x5c, 0xeb, 0xf9, 0xff, 0xe6, 0xeb, 0x6f, 0xbd,
            0xf6, 0xe2, 0x4b, 0x77, 0x3f, 0x72, 0xeb, 0xe5, 0x17, 0x5f, 0x62, 0xff, 0x26, 0xbf,
            0x78, 0xee, 0xc5, 0x7b, 0xaf, 0xdd, 0x78, 0xee, 0x6b, 0x5f, 0x7e, 0xfe, 0xee, 0x2b,
            0x2f, 0x5b, 0x1d, 0x2b, 0xfe, 0x51, 0x00,
        ];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 4096];
        // Should not panic — error or success, either is fine
        let _ = d.deflate_decompress(data, &mut output, enough::Unstoppable);
    }

    /// miniz_oxide #143: sync flush marker in raw deflate (WebSocket per-message
    /// compression). The block is non-final, so whole-buffer decompressor rejects it.
    #[test]
    fn sync_flush_nonfinal_block_rejected() {
        // "Hello" compressed with sync flush: non-final dynamic block + sync marker
        let data: &[u8] = &[
            0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00, 0x00, 0xff, 0xff,
        ];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 256];
        // Whole-buffer decompressor requires a final block — this has none
        assert!(
            d.deflate_decompress(data, &mut output, enough::Unstoppable)
                .is_err()
        );
    }

    /// zlib-rs #172: gzip stream that failed at certain chunk sizes.
    /// Whole-buffer decompressor should handle this correctly.
    #[test]
    fn gzip_test_vector_zlibrs172() {
        let data: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 75, 173, 40, 72, 77, 46, 73, 77, 81, 200, 47, 45, 41,
            40, 45, 1, 0, 176, 1, 57, 179, 15, 0, 0, 0,
        ];
        let mut d = Decompressor::new();
        let mut output = vec![0u8; 256];
        let result = d
            .gzip_decompress(data, &mut output, enough::Unstoppable)
            .expect("valid gzip stream should decompress");
        assert_eq!(
            &output[..result.output_written],
            b"expected output",
            "gzip test vector should decode to 'expected output'"
        );
    }

    // =====================================================================
    // input_consumed correctness tests (libdeflate #420, miniz_oxide #158)
    // =====================================================================

    /// One-shot decode under every SIMD tier the CPU has, which covers the
    /// x86-64-v4 build on AVX-512 machines and the default build without it.
    /// Inputs straddle `ONESHOT_V4_MIN_INPUT`; output buffers are exact and
    /// have slack (the fastloop ends differently in each).
    #[test]
    #[cfg(feature = "simd")]
    fn oneshot_all_dispatch_tiers() {
        use archmage::testing::{CompileTimePolicy, for_each_token_permutation};

        let mut x = 0x1234_5678u32;
        let mut make = |len: usize| -> Vec<u8> {
            (0..len)
                .map(|i| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    match (i / 4096) % 3 {
                        0 => (i % 251) as u8,
                        1 => (i / 64) as u8,
                        _ => x as u8,
                    }
                })
                .collect()
        };
        let cases: Vec<(Vec<u8>, Vec<u8>)> = [1_000usize, 20_000, 40_000, 300_000]
            .into_iter()
            .map(|len| {
                let data = make(len);
                let mut c =
                    libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
                let mut z = vec![0u8; c.zlib_compress_bound(data.len())];
                let n = c.zlib_compress(&data, &mut z).unwrap();
                z.truncate(n);
                (data, z)
            })
            .collect();
        // Both sides of the v4 threshold are exercised.
        assert!(cases.iter().any(|(_, z)| z.len() < 16 * 1024));
        assert!(cases.iter().any(|(_, z)| z.len() >= 16 * 1024));

        let report = for_each_token_permutation(CompileTimePolicy::Warn, |perm| {
            for (data, z) in &cases {
                for slack in [0usize, 1000] {
                    let mut out = vec![0u8; data.len() + slack];
                    let r = Decompressor::new()
                        .zlib_decompress(z, &mut out, enough::Unstoppable)
                        .unwrap();
                    assert_eq!(r.output_written, data.len(), "tier: {perm}");
                    assert!(
                        out[..data.len()] == data[..],
                        "len {}, slack {slack}, tier: {perm}",
                        data.len()
                    );
                }
            }
        });
        eprintln!("one-shot permutation test: {report}");
    }
}

/// Tests that round-trip through the crate's own `Compressor` (the tests
/// above decode fixed bytes or libdeflater-compressed data and stay live
/// in decode-only builds).
#[cfg(all(test, feature = "compress", not(miri), not(target_arch = "wasm32")))]
mod compress_roundtrip_tests {
    use super::*;

    /// flate2 #474: empty input with L0 compression.
    /// Verify compress + decompress round-trip works for empty data at level 0.
    #[test]
    fn empty_input_level0_roundtrip() {
        use crate::{CompressionLevel, Compressor};

        let mut compressor = Compressor::new(CompressionLevel::none());

        // deflate
        let bound = Compressor::deflate_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = compressor
            .deflate_compress(&[], &mut compressed, enough::Unstoppable)
            .unwrap();
        assert!(csize > 0, "deflate L0 should produce non-empty output");

        let mut d = Decompressor::new();
        let mut output = vec![0u8; 0];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 0);

        // zlib
        let bound = Compressor::zlib_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = compressor
            .zlib_compress(&[], &mut compressed, enough::Unstoppable)
            .unwrap();
        let mut output = vec![0u8; 0];
        let result = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 0);

        // gzip
        let bound = Compressor::gzip_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = compressor
            .gzip_compress(&[], &mut compressed, enough::Unstoppable)
            .unwrap();
        let mut output = vec![0u8; 0];
        let result = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 0);
    }

    /// zlib stream with invalid checksum must be rejected.
    /// Verifies Adler-32 footer is actually checked (flate2 #258).
    #[test]
    fn reject_zlib_bad_adler32() {
        use crate::{CompressionLevel, Compressor};

        let data = b"Hello, World!";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .zlib_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        // Corrupt the last byte (part of Adler-32 footer)
        compressed[csize - 1] ^= 0xFF;

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let err = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::ChecksumMismatch);
    }

    /// gzip stream with corrupted CRC-32 must be rejected.
    #[test]
    fn reject_gzip_bad_crc32() {
        use crate::{CompressionLevel, Compressor};

        let data = b"Hello, World!";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .gzip_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        // Corrupt the CRC-32 (4 bytes before the last 4 ISIZE bytes)
        compressed[csize - 5] ^= 0xFF;

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let err = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::ChecksumMismatch);
    }

    /// gzip stream with corrupted ISIZE must be rejected.
    #[test]
    fn reject_gzip_bad_isize() {
        use crate::{CompressionLevel, Compressor};

        let data = b"Hello, World!";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .gzip_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        // Corrupt the ISIZE (last 4 bytes)
        compressed[csize - 1] ^= 0xFF;

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        assert!(
            d.gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .is_err()
        );
    }

    /// Exact-size compress buffer: verify compress_bound is sufficient for
    /// all levels and that the buffer is fully used (libdeflate #294, #102).
    #[test]
    fn compress_bound_exact_buffer_all_levels() {
        use crate::{CompressionLevel, Compressor};

        // Test several data patterns at every level
        let patterns: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("one_byte", vec![42]),
            ("zeros_1k", vec![0; 1024]),
            ("sequential_1k", (0..=255u8).cycle().take(1024).collect()),
            ("random_ish", {
                // Pseudo-random via simple LCG
                let mut v = vec![0u8; 4096];
                let mut s: u32 = 0xDEAD_BEEF;
                for b in v.iter_mut() {
                    s = s.wrapping_mul(1103515245).wrapping_add(12345);
                    *b = (s >> 16) as u8;
                }
                v
            }),
        ];

        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let mut d = Decompressor::new();
            for (name, data) in &patterns {
                // deflate
                let bound = Compressor::deflate_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c
                    .deflate_compress(data, &mut compressed, enough::Unstoppable)
                    .unwrap_or_else(|e| panic!("deflate L{level} {name}: compress failed: {e:?}"));
                assert!(csize <= bound, "deflate L{level} {name}: exceeded bound");

                let mut output = vec![0u8; data.len().max(1)];
                let result = d
                    .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                    .unwrap_or_else(|e| {
                        panic!("deflate L{level} {name}: decompress failed: {e:?}")
                    });
                assert_eq!(
                    &output[..result.output_written],
                    data.as_slice(),
                    "deflate L{level} {name}: roundtrip mismatch"
                );

                // zlib
                let bound = Compressor::zlib_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c
                    .zlib_compress(data, &mut compressed, enough::Unstoppable)
                    .unwrap_or_else(|e| panic!("zlib L{level} {name}: compress failed: {e:?}"));
                assert!(csize <= bound, "zlib L{level} {name}: exceeded bound");

                // gzip
                let bound = Compressor::gzip_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c
                    .gzip_compress(data, &mut compressed, enough::Unstoppable)
                    .unwrap_or_else(|e| panic!("gzip L{level} {name}: compress failed: {e:?}"));
                assert!(csize <= bound, "gzip L{level} {name}: exceeded bound");
            }
        }
    }

    /// Zero-length output buffer: decompress of valid data into empty output
    /// must not panic (zlib-rs #23). Empty data compresses to a final empty
    /// stored block or a dynamic block encoding zero literals.
    #[test]
    fn decompress_into_zero_length_output() {
        use crate::{CompressionLevel, Compressor};

        let data: &[u8] = &[];
        let mut c = Compressor::new(CompressionLevel::new(0));
        let bound = Compressor::deflate_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new();
        let mut output: Vec<u8> = vec![];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 0);
    }

    /// Reuse decompressor across many operations: verifies state reset works.
    /// Catches stale-state bugs where previous block's tables bleed through.
    #[test]
    fn decompressor_reuse_across_formats() {
        use crate::{CompressionLevel, Compressor};

        let mut c = Compressor::new(CompressionLevel::new(6));
        let mut d = Decompressor::new();

        let datasets: &[&[u8]] = &[
            b"Hello",
            b"",
            &[0u8; 10000],
            &(0..=255u8).cycle().take(5000).collect::<Vec<_>>(),
            b"a",
        ];

        for data in datasets {
            // deflate
            let bound = Compressor::deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .deflate_compress(data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let mut output = vec![0u8; data.len().max(1)];
            let result = d
                .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(&output[..result.output_written], *data);

            // zlib
            let bound = Compressor::zlib_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .zlib_compress(data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let mut output = vec![0u8; data.len().max(1)];
            let result = d
                .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(&output[..result.output_written], *data);

            // gzip
            let bound = Compressor::gzip_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .gzip_compress(data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let mut output = vec![0u8; data.len().max(1)];
            let result = d
                .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(&output[..result.output_written], *data);
        }
    }

    /// Exact compressed size buffer: compression must succeed when output buffer
    /// is exactly the compressed size (not just compress_bound).
    /// Catches libdeflate #102 where exact-fit buffers failed.
    #[test]
    fn compress_exact_output_size() {
        use crate::{CompressionLevel, Compressor};

        let data = b"1234567890";
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            // First compress with generous buffer to learn the exact size
            let bound = Compressor::deflate_compress_bound(data.len());
            let mut big_buf = vec![0u8; bound];
            let exact_size = c
                .deflate_compress(data, &mut big_buf, enough::Unstoppable)
                .unwrap();

            // Now compress into a buffer of exactly that size
            let mut exact_buf = vec![0u8; exact_size];
            let result_size = c
                .deflate_compress(data, &mut exact_buf, enough::Unstoppable)
                .unwrap_or_else(|e| {
                    panic!("L{level}: compress into exact-size buffer failed: {e:?}")
                });
            assert_eq!(result_size, exact_size, "L{level}: size mismatch");
            assert_eq!(
                &exact_buf[..result_size],
                &big_buf[..exact_size],
                "L{level}: output differs"
            );
        }
    }

    /// Deterministic compression: reusing a Compressor must produce identical
    /// output for identical input (zlib-rs #459 pattern).
    #[test]
    fn compression_deterministic_across_reuse() {
        use crate::{CompressionLevel, Compressor};

        let data: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::deflate_compress_bound(data.len());

            let mut out1 = vec![0u8; bound];
            let size1 = c
                .deflate_compress(&data, &mut out1, enough::Unstoppable)
                .unwrap();

            let mut out2 = vec![0u8; bound];
            let size2 = c
                .deflate_compress(&data, &mut out2, enough::Unstoppable)
                .unwrap();

            assert_eq!(size1, size2, "L{level}: sizes differ on reuse");
            assert_eq!(
                &out1[..size1],
                &out2[..size2],
                "L{level}: output differs on reuse"
            );
        }
    }

    /// deflate: input_consumed must equal the compressed size (no trailing bytes).
    #[test]
    fn input_consumed_deflate_exact() {
        use crate::{CompressionLevel, Compressor};

        let data = b"The quick brown fox jumps over the lazy dog.";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, data.len());
    }

    /// deflate: input_consumed ignores trailing garbage after the final block.
    #[test]
    fn input_consumed_deflate_trailing_garbage() {
        use crate::{CompressionLevel, Compressor};

        let data = b"Hello, World!";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound + 100];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        // Append 100 bytes of garbage after the compressed data
        for i in 0..100 {
            compressed[csize + i] = 0xAB;
        }

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let result = d
            .deflate_decompress(&compressed[..csize + 100], &mut output, enough::Unstoppable)
            .unwrap();
        // Must report only the DEFLATE bytes consumed, not the trailing garbage
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, data.len());
        assert_eq!(&output[..result.output_written], &data[..]);
    }

    /// zlib: input_consumed must include header (2) + deflate + footer (4).
    #[test]
    fn input_consumed_zlib() {
        use crate::{CompressionLevel, Compressor};

        let data = b"The quick brown fox jumps over the lazy dog.";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .zlib_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let result = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, data.len());
    }

    /// gzip: input_consumed must include header (10+) + deflate + footer (8).
    #[test]
    fn input_consumed_gzip() {
        use crate::{CompressionLevel, Compressor};

        let data = b"The quick brown fox jumps over the lazy dog.";
        let mut c = Compressor::new(CompressionLevel::new(6));
        let bound = Compressor::gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .gzip_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new();
        let mut output = vec![0u8; data.len()];
        let result = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, data.len());
    }

    /// input_consumed for empty data across all formats.
    #[test]
    fn input_consumed_empty_data() {
        use crate::{CompressionLevel, Compressor};

        let data: &[u8] = &[];
        let mut c = Compressor::new(CompressionLevel::new(0));
        let mut d = Decompressor::new();

        // deflate
        let bound = Compressor::deflate_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();
        let mut output = vec![0u8; 1];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, 0);

        // zlib
        let bound = Compressor::zlib_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c
            .zlib_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();
        let result = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, 0);

        // gzip
        let bound = Compressor::gzip_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c
            .gzip_compress(data, &mut compressed, enough::Unstoppable)
            .unwrap();
        let result = d
            .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.input_consumed, csize);
        assert_eq!(result.output_written, 0);
    }

    /// input_consumed at all compression levels, all formats.
    #[test]
    fn input_consumed_all_levels() {
        use crate::{CompressionLevel, Compressor};

        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        for level in 0..=12 {
            let mut c = Compressor::new(CompressionLevel::new(level));
            let mut d = Decompressor::new();
            let mut output = vec![0u8; data.len()];

            // deflate
            let bound = Compressor::deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .deflate_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let result = d
                .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(result.input_consumed, csize, "L{level} deflate");
            assert_eq!(result.output_written, data.len(), "L{level} deflate");

            // zlib
            let bound = Compressor::zlib_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .zlib_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let result = d
                .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(result.input_consumed, csize, "L{level} zlib");
            assert_eq!(result.output_written, data.len(), "L{level} zlib");

            // gzip
            let bound = Compressor::gzip_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c
                .gzip_compress(&data, &mut compressed, enough::Unstoppable)
                .unwrap();
            let result = d
                .gzip_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
                .unwrap();
            assert_eq!(result.input_consumed, csize, "L{level} gzip");
            assert_eq!(result.output_written, data.len(), "L{level} gzip");
        }
    }

    #[test]
    fn max_output_size_allows_within_limit() {
        // Data that decompresses to 1000 bytes should succeed with limit >= 1000
        let data: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let mut c = crate::Compressor::new(crate::CompressionLevel::fastest());
        let bound = crate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new().with_max_output_size(Some(1000));
        let mut output = vec![0u8; 2000];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 1000);
    }

    #[test]
    fn max_output_size_rejects_over_limit() {
        // Data that decompresses to 1000 bytes should fail with limit < 1000
        let data: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let mut c = crate::Compressor::new(crate::CompressionLevel::fastest());
        let bound = crate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new().with_max_output_size(Some(500));
        let mut output = vec![0u8; 2000];
        let err = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::OutputLimitExceeded);
    }

    #[test]
    fn max_output_size_none_is_unlimited() {
        // Default (None) should behave exactly like before
        let data: Vec<u8> = vec![0u8; 65536];
        let mut c = crate::Compressor::new(crate::CompressionLevel::fastest());
        let bound = crate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new(); // no limit set
        let mut output = vec![0u8; 65536];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 65536);
    }

    #[test]
    fn max_output_size_works_with_zlib_wrapper() {
        let data: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let mut c = crate::Compressor::new(crate::CompressionLevel::fastest());
        let bound = crate::Compressor::zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .zlib_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        // Should fail: limit is too small
        let mut d = Decompressor::new().with_max_output_size(Some(500));
        let mut output = vec![0u8; 2000];
        let err = d
            .zlib_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap_err();
        assert_eq!(err, DecompressionError::OutputLimitExceeded);
    }

    #[test]
    fn max_output_size_exact_boundary() {
        // Limit exactly equals output size should succeed
        let data: Vec<u8> = (0..100).collect();
        let mut c = crate::Compressor::new(crate::CompressionLevel::fastest());
        let bound = crate::Compressor::deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c
            .deflate_compress(&data, &mut compressed, enough::Unstoppable)
            .unwrap();

        let mut d = Decompressor::new().with_max_output_size(Some(100));
        let mut output = vec![0u8; 200];
        let result = d
            .deflate_decompress(&compressed[..csize], &mut output, enough::Unstoppable)
            .unwrap();
        assert_eq!(result.output_written, 100);
    }
}

/// Compressed input below which the one-shot decoder skips its x86-64-v4
/// build (see `deflate_decompress_core`).
#[cfg(all(feature = "avx512", target_arch = "x86_64"))]
const ONESHOT_V4_MIN_INPUT: usize = 16 * 1024;

/// x86-64-v4 build of the one-shot decode loop.
#[cfg(all(feature = "avx512", target_arch = "x86_64"))]
mod oneshot_v4 {
    use super::*;
    use archmage::prelude::*;

    #[arcane]
    pub(super) fn core_v4(
        _token: X64V4Token,
        d: &mut Decompressor,
        input: &[u8],
        output: &mut [u8],
        stop: &impl enough::Stop,
    ) -> Result<(usize, usize), DecompressionError> {
        d.deflate_decompress_core_impl::<false>(input, output, usize::MAX, &mut 0, stop)
            .map(|(i, o, _)| (i, o))
    }
}

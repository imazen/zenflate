//! Streaming DEFLATE/zlib/gzip decompression.
//!
//! Provides [`StreamDecompressor`], a pull-based streaming decompressor that
//! works with any input source via the [`InputSource`] trait. Supports
//! `no_std + alloc` environments.
//!
//! # Cancellation
//!
//! Unlike the whole-buffer [`Decompressor`](super::Decompressor), the streaming
//! API doesn't take a `Stop` parameter — the caller controls the loop and can
//! check cancellation between `fill()` calls. Each `fill()` call is bounded by
//! the output capacity, so it completes quickly.

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{DecompressionError, StreamError};

use super::{
    DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN, DEFLATE_BLOCKTYPE_STATIC_HUFFMAN,
    DEFLATE_BLOCKTYPE_UNCOMPRESSED, DEFLATE_MAX_PRE_CODEWORD_LEN, DEFLATE_NUM_PRECODE_SYMS,
    DEFLATE_PRECODE_LENS_PERMUTATION, Decompressor, FASTLOOP_MAX_BYTES_READ,
    FASTLOOP_MAX_BYTES_WRITTEN, GZIP_CM_DEFLATE, GZIP_FCOMMENT, GZIP_FEXTRA, GZIP_FHCRC,
    GZIP_FNAME, GZIP_FRESERVED, GZIP_ID1, GZIP_ID2, HUFFDEC_END_OF_BLOCK, HUFFDEC_EXCEPTIONAL,
    HUFFDEC_LITERAL, HUFFDEC_SUBTABLE_POINTER, LITLEN_DECODE_RESULTS, LITLEN_TABLEBITS,
    OFFSET_DECODE_RESULTS, OFFSET_TABLEBITS, PRECODE_DECODE_RESULTS, PRECODE_TABLEBITS,
    ZLIB_CINFO_32K_WINDOW, ZLIB_CM_DEFLATE, bitmask, build_decode_table, extract_varbits,
    extract_varbits8, refill_bits, refill_bits_fast, table_lookup,
};

// ---------------------------------------------------------------------------
// InputSource trait
// ---------------------------------------------------------------------------

/// Buffered input source for streaming decompression.
///
/// This trait abstracts over different input sources (slices, `BufRead`, etc.)
/// with zero overhead when monomorphized. The design mirrors `BufRead` but
/// works in `no_std + alloc` environments.
pub trait InputSource {
    /// Error type for this source. Use `core::convert::Infallible` for infallible sources.
    type Error;

    /// Return a reference to available input bytes.
    ///
    /// Returns an empty slice when no more data is available (EOF).
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error>;

    /// Mark `n` bytes as consumed. Must not exceed the length returned by `fill_buf`.
    fn consume(&mut self, n: usize);
}

/// `&[u8]` as an infallible, zero-cost input source.
impl InputSource for &[u8] {
    type Error = core::convert::Infallible;

    #[inline(always)]
    fn fill_buf(&mut self) -> Result<&[u8], core::convert::Infallible> {
        Ok(*self)
    }

    #[inline(always)]
    fn consume(&mut self, n: usize) {
        *self = &self[n..];
    }
}

/// Wraps a [`std::io::BufRead`] as an [`InputSource`].
#[cfg(feature = "std")]
pub struct BufReadSource<R>(pub R);

#[cfg(feature = "std")]
impl<R: std::io::BufRead> InputSource for BufReadSource<R> {
    type Error = std::io::Error;

    #[inline]
    fn fill_buf(&mut self) -> Result<&[u8], std::io::Error> {
        std::io::BufRead::fill_buf(&mut self.0)
    }

    #[inline]
    fn consume(&mut self, n: usize) {
        std::io::BufRead::consume(&mut self.0, n);
    }
}

// ---------------------------------------------------------------------------
// Wrapper format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapperFormat {
    Raw,
    Zlib,
    Gzip,
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    WrapperHeader,
    BlockHeader,
    DynamicPrecodeLens {
        num_litlen_syms: usize,
        num_offset_syms: usize,
        num_explicit_precode_lens: usize,
        precode_idx: usize,
    },
    DynamicCodeLengths {
        num_litlen_syms: usize,
        num_offset_syms: usize,
    },
    CompressedData,
    UncompressedData {
        remaining: usize,
    },
    WrapperFooter,
    Done,
}

#[derive(Debug, Clone, Copy)]
struct PendingMatch {
    offset: usize,
    length: usize,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of the internal input staging buffer.
const INPUT_BUF_SIZE: usize = 512;

/// Minimum lookback required for match references (32KB window).
const LOOKBACK_SIZE: usize = 32 * 1024;

/// Default output buffer capacity for [`StreamDecompressor`]: 64 KiB.
///
/// Good for general-purpose streaming. For scanline-at-a-time workloads
/// (e.g. PNG), use `2 * row_stride` instead.
pub const DEFAULT_CAPACITY: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// StreamDecompressor
// ---------------------------------------------------------------------------

/// Pull-based streaming DEFLATE/zlib/gzip decompressor.
///
/// Generic over any [`InputSource`]. For `&[u8]`, this has zero overhead
/// from the source abstraction (the `Result<_, Infallible>` is eliminated).
///
/// # Capacity
///
/// The `capacity` parameter controls how many bytes of decompressed output
/// can be buffered before the caller must drain via [`peek()`](Self::peek) /
/// [`advance()`](Self::advance). Internally, the buffer is `32 KiB lookback + capacity`.
///
/// - For **scanline-at-a-time** (e.g. PNG): `2 * row_stride`
/// - For **general streaming**: [`DEFAULT_CAPACITY`] (64 KiB)
/// - Larger values mean fewer compaction cycles; smaller values use less memory.
///
/// # Usage
///
/// ```no_run
/// # use zenflate::decompress::streaming::{StreamDecompressor, InputSource, DEFAULT_CAPACITY};
/// # let compressed_data: &[u8] = &[];
/// let mut dec = StreamDecompressor::deflate(compressed_data, DEFAULT_CAPACITY);
/// while !dec.is_done() {
///     dec.fill().unwrap();
///     let data = dec.peek();
///     // process data...
///     let n = data.len();
///     dec.advance(n);
/// }
/// ```
pub struct StreamDecompressor<S> {
    source: S,
    inner: Decompressor,

    // Output buffer: [lookback (32KB) | peekable (capacity)]
    buffer: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    read_pos: usize,

    // Internal input staging buffer
    input_buf: Vec<u8>,
    input_len: usize,
    input_pos: usize,

    // Bitstream state
    bitbuf: u64,
    bitsleft: u32,
    overread_count: usize,

    // State machine
    state: StreamState,
    is_final_block: bool,
    pending_match: Option<PendingMatch>,
    pending_literal: Option<u8>,

    // Wrapper format
    wrapper: WrapperFormat,

    // Checksum (Adler-32 for zlib, CRC-32 for gzip)
    checksum: u32,
    total_output: u64,
    // Position in buffer up to which checksum has been computed
    checksum_watermark: usize,

    // Checksum leniency
    skip_checksum: bool,
    checksum_matched: Option<bool>,
}

impl<S> StreamDecompressor<S> {
    /// Consume the decompressor and return the underlying input source.
    pub fn into_inner(self) -> S {
        self.source
    }

    /// When true, checksum mismatches in zlib/gzip wrappers are recorded
    /// instead of returning an error. The decompressed data is still returned.
    ///
    /// After decompression completes ([`is_done()`](Self::is_done) returns
    /// true), call [`checksum_matched()`](Self::checksum_matched) to see if
    /// the checksum was correct.
    #[must_use]
    pub fn with_skip_checksum(mut self, skip: bool) -> Self {
        self.skip_checksum = skip;
        self
    }

    /// Whether the wrapper checksum matched after decompression.
    ///
    /// - `None` — footer not yet processed (raw DEFLATE or stream not finished)
    /// - `Some(true)` — checksum matched
    /// - `Some(false)` — checksum mismatch (only possible when `skip_checksum` is set)
    pub fn checksum_matched(&self) -> Option<bool> {
        self.checksum_matched
    }
}

impl<S: InputSource> StreamDecompressor<S> {
    fn new(source: S, capacity: usize, wrapper: WrapperFormat) -> Self {
        assert!(capacity > 0, "capacity must be at least 1");
        let buf_size = LOOKBACK_SIZE + capacity;
        let initial_state = if wrapper == WrapperFormat::Raw {
            StreamState::BlockHeader
        } else {
            StreamState::WrapperHeader
        };
        let checksum_init = match wrapper {
            WrapperFormat::Zlib => 1, // Adler-32 starts at 1
            _ => 0,
        };
        Self {
            source,
            inner: Decompressor::new(),
            buffer: vec![0u8; buf_size],
            capacity,
            write_pos: LOOKBACK_SIZE,
            read_pos: LOOKBACK_SIZE,
            input_buf: vec![0u8; INPUT_BUF_SIZE],
            input_len: 0,
            input_pos: 0,
            bitbuf: 0,
            bitsleft: 0,
            overread_count: 0,
            state: initial_state,
            is_final_block: false,
            pending_match: None,
            pending_literal: None,
            wrapper,
            checksum: checksum_init,
            total_output: 0,
            checksum_watermark: LOOKBACK_SIZE,
            skip_checksum: false,
            checksum_matched: None,
        }
    }

    /// Create a streaming decompressor for raw DEFLATE data.
    ///
    /// `capacity` is the maximum bytes of decompressed output buffered
    /// between [`peek()`](Self::peek)/[`advance()`](Self::advance) calls.
    /// Use [`DEFAULT_CAPACITY`] if unsure.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn deflate(source: S, capacity: usize) -> Self {
        Self::new(source, capacity, WrapperFormat::Raw)
    }

    /// Create a streaming decompressor for zlib-wrapped data.
    ///
    /// See [`deflate()`](Self::deflate) for the meaning of `capacity`.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn zlib(source: S, capacity: usize) -> Self {
        Self::new(source, capacity, WrapperFormat::Zlib)
    }

    /// Create a streaming decompressor for gzip-wrapped data.
    ///
    /// See [`deflate()`](Self::deflate) for the meaning of `capacity`.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn gzip(source: S, capacity: usize) -> Self {
        Self::new(source, capacity, WrapperFormat::Gzip)
    }

    /// Borrow all available decompressed output. Zero-copy.
    #[inline]
    pub fn peek(&self) -> &[u8] {
        &self.buffer[self.read_pos..self.write_pos]
    }

    /// Mark `n` bytes of output as consumed.
    ///
    /// # Panics
    ///
    /// Panics if `n` exceeds the length of `peek()`.
    #[inline]
    pub fn advance(&mut self, n: usize) {
        assert!(
            n <= self.write_pos - self.read_pos,
            "advance({n}) exceeds available output ({})",
            self.write_pos - self.read_pos
        );
        self.read_pos += n;
    }

    /// Returns `true` when the stream is fully decompressed and checksums verified.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.state == StreamState::Done
    }

    /// Reset the decompressor for a new stream of the same format,
    /// keeping buffer allocations.
    ///
    /// The wrapper format (DEFLATE/zlib/gzip) is preserved from construction.
    /// To switch formats, construct a new `StreamDecompressor` instead.
    pub fn reset(&mut self, source: S) {
        let wrapper = self.wrapper;
        let checksum_init = match wrapper {
            WrapperFormat::Zlib => 1,
            _ => 0,
        };
        self.source = source;
        self.inner = Decompressor::new();
        self.write_pos = LOOKBACK_SIZE;
        self.read_pos = LOOKBACK_SIZE;
        self.input_len = 0;
        self.input_pos = 0;
        self.bitbuf = 0;
        self.bitsleft = 0;
        self.overread_count = 0;
        self.state = if wrapper == WrapperFormat::Raw {
            StreamState::BlockHeader
        } else {
            StreamState::WrapperHeader
        };
        self.is_final_block = false;
        self.pending_match = None;
        self.pending_literal = None;
        self.checksum = checksum_init;
        self.total_output = 0;
        self.checksum_watermark = LOOKBACK_SIZE;
        // Preserve skip_checksum across reset; clear result
        self.checksum_matched = None;
    }

    // -----------------------------------------------------------------------
    // Internal: staging buffer management
    // -----------------------------------------------------------------------

    /// Compact the staging buffer and pull more data from the source.
    /// Safe to call at any time — preserves bytes from input_pos onward.
    fn fill_input(&mut self) -> Result<(), S::Error> {
        // Compact: move unconsumed data to front
        if self.input_pos > 0 {
            self.input_buf
                .copy_within(self.input_pos..self.input_len, 0);
            self.input_len -= self.input_pos;
            self.input_pos = 0;
        }
        // Fill remaining space from source
        while self.input_len < INPUT_BUF_SIZE {
            let src = self.source.fill_buf()?;
            if src.is_empty() {
                break;
            }
            let can_copy = src.len().min(INPUT_BUF_SIZE - self.input_len);
            self.input_buf[self.input_len..self.input_len + can_copy]
                .copy_from_slice(&src[..can_copy]);
            self.input_len += can_copy;
            self.source.consume(can_copy);
        }
        Ok(())
    }

    /// Ensure the staging buffer has at least `min_bytes` available.
    /// Returns false only if source is exhausted and fewer bytes remain.
    fn ensure_input_bytes(&mut self, min_bytes: usize) -> Result<bool, S::Error> {
        let available = self.input_len - self.input_pos;
        if available >= min_bytes {
            return Ok(true);
        }
        self.fill_input()?;
        Ok(self.input_len - self.input_pos >= min_bytes)
    }

    // -----------------------------------------------------------------------
    // Internal: output buffer management
    // -----------------------------------------------------------------------

    /// Compact the output buffer. Keeps the last 32KB as lookback.
    /// Must be called only after updating checksum up to write_pos.
    fn compact_output(&mut self) {
        if self.read_pos <= LOOKBACK_SIZE {
            return;
        }
        // Update checksum for any data not yet checksummed before compaction
        self.flush_checksum();

        let keep_start = self.read_pos.saturating_sub(LOOKBACK_SIZE);
        let keep_len = self.write_pos - keep_start;
        self.buffer.copy_within(keep_start..self.write_pos, 0);
        self.read_pos -= keep_start;
        self.write_pos = keep_len;
        self.checksum_watermark = self.write_pos;
    }

    fn output_space(&self) -> usize {
        self.buffer.len() - self.write_pos
    }

    fn peek_len(&self) -> usize {
        self.write_pos - self.read_pos
    }

    // -----------------------------------------------------------------------
    // Internal: checksum tracking
    // -----------------------------------------------------------------------

    /// Update checksum for all output written since last flush.
    fn flush_checksum(&mut self) {
        if self.wrapper == WrapperFormat::Raw || self.checksum_watermark >= self.write_pos {
            return;
        }
        let data = &self.buffer[self.checksum_watermark..self.write_pos];
        match self.wrapper {
            WrapperFormat::Zlib => {
                self.checksum = crate::checksum::adler32(self.checksum, data);
            }
            WrapperFormat::Gzip => {
                self.checksum = crate::checksum::crc32(self.checksum, data);
            }
            WrapperFormat::Raw => {}
        }
        self.total_output += data.len() as u64;
        self.checksum_watermark = self.write_pos;
    }

    // -----------------------------------------------------------------------
    // fill() — the main entry point
    // -----------------------------------------------------------------------

    /// Pull from source, decompress into internal buffer.
    ///
    /// Returns a reference to available decompressed output (same as `peek()`).
    /// Call `peek()`/`advance()` to consume output, then `fill()` again.
    pub fn fill(&mut self) -> Result<&[u8], StreamError<S::Error>> {
        if self.peek_len() >= self.capacity || self.state == StreamState::Done {
            return Ok(self.peek());
        }

        if self.output_space() == 0 {
            self.compact_output();
        }

        loop {
            let prev_write_pos = self.write_pos;

            match self.state {
                StreamState::WrapperHeader => {
                    self.parse_wrapper_header()?;
                }
                StreamState::BlockHeader => {
                    self.fill_input().map_err(StreamError::Source)?;
                    self.parse_block_header()?;
                }
                StreamState::DynamicPrecodeLens {
                    num_litlen_syms,
                    num_offset_syms,
                    num_explicit_precode_lens,
                    precode_idx,
                } => {
                    self.fill_input().map_err(StreamError::Source)?;
                    self.parse_dynamic_precode_lens(
                        num_litlen_syms,
                        num_offset_syms,
                        num_explicit_precode_lens,
                        precode_idx,
                    )?;
                }
                StreamState::DynamicCodeLengths {
                    num_litlen_syms,
                    num_offset_syms,
                } => {
                    self.fill_input().map_err(StreamError::Source)?;
                    self.parse_dynamic_code_lengths(num_litlen_syms, num_offset_syms)?;
                }
                StreamState::CompressedData => {
                    self.decompress_block()?;
                }
                StreamState::UncompressedData { remaining } => {
                    self.copy_uncompressed(remaining)?;
                }
                StreamState::WrapperFooter => {
                    self.parse_wrapper_footer()?;
                }
                StreamState::Done => {
                    return Ok(self.peek());
                }
            }

            if self.peek_len() >= self.capacity || self.state == StreamState::Done {
                return Ok(self.peek());
            }

            // If no output progress was made and we have data to return,
            // return to the caller so they can drain and we can compact.
            if self.write_pos == prev_write_pos && self.peek_len() > 0 {
                return Ok(self.peek());
            }

            if self.output_space() == 0 {
                self.compact_output();
                if self.output_space() == 0 {
                    return Ok(self.peek());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Wrapper header parsing
    // -----------------------------------------------------------------------

    fn parse_wrapper_header(&mut self) -> Result<(), StreamError<S::Error>> {
        self.fill_input().map_err(StreamError::Source)?;
        let input = &self.input_buf[self.input_pos..self.input_len];

        match self.wrapper {
            WrapperFormat::Zlib => {
                if input.len() < 2 {
                    return Err(StreamError::Decompress(DecompressionError::InvalidHeader));
                }
                let hdr = u16::from_be_bytes([input[0], input[1]]);
                if !hdr.is_multiple_of(31)
                    || (input[0] & 0xF) != ZLIB_CM_DEFLATE
                    || (input[0] >> 4) > ZLIB_CINFO_32K_WINDOW
                    || (input[1] >> 5) & 1 != 0
                {
                    return Err(StreamError::Decompress(DecompressionError::InvalidHeader));
                }
                self.input_pos += 2;
                self.state = StreamState::BlockHeader;
            }
            WrapperFormat::Gzip => {
                self.parse_gzip_header()?;
            }
            WrapperFormat::Raw => {
                self.state = StreamState::BlockHeader;
            }
        }
        Ok(())
    }

    fn parse_gzip_header(&mut self) -> Result<(), StreamError<S::Error>> {
        let bad = StreamError::Decompress(DecompressionError::InvalidHeader);
        let input = &self.input_buf[self.input_pos..self.input_len];

        if input.len() < 10 {
            return Err(bad);
        }
        if input[0] != GZIP_ID1
            || input[1] != GZIP_ID2
            || input[2] != GZIP_CM_DEFLATE
            || input[3] & GZIP_FRESERVED != 0
        {
            return Err(bad);
        }
        let flg = input[3];
        let mut pos = 10;

        if flg & GZIP_FEXTRA != 0 {
            if pos + 2 > input.len() {
                return Err(bad);
            }
            let xlen = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
            pos += 2;
            if pos + xlen > input.len() {
                return Err(bad);
            }
            pos += xlen;
        }
        if flg & GZIP_FNAME != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            if pos >= input.len() {
                return Err(bad);
            }
            pos += 1;
        }
        if flg & GZIP_FCOMMENT != 0 {
            while pos < input.len() && input[pos] != 0 {
                pos += 1;
            }
            if pos >= input.len() {
                return Err(bad);
            }
            pos += 1;
        }
        if flg & GZIP_FHCRC != 0 {
            if pos + 2 > input.len() {
                return Err(bad);
            }
            pos += 2;
        }

        self.input_pos += pos;
        self.state = StreamState::BlockHeader;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Block header parsing
    // -----------------------------------------------------------------------

    fn parse_block_header(&mut self) -> Result<(), StreamError<S::Error>> {
        let bad = DecompressionError::BadData;
        let input = &self.input_buf[..self.input_len];

        refill_bits(
            &mut self.bitbuf,
            &mut self.bitsleft,
            input,
            &mut self.input_pos,
            &mut self.overread_count,
        )?;

        self.is_final_block = (self.bitbuf & 1) != 0;
        let block_type = ((self.bitbuf >> 1) & 3) as u32;

        if block_type == DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN {
            let num_litlen_syms = 257 + ((self.bitbuf >> 3) & bitmask(5)) as usize;
            let num_offset_syms = 1 + ((self.bitbuf >> 8) & bitmask(5)) as usize;
            let num_explicit_precode_lens = 4 + ((self.bitbuf >> 13) & bitmask(4)) as usize;

            self.inner.static_codes_loaded = false;

            self.inner.precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[0] as usize] =
                ((self.bitbuf >> 17) & 7) as u8;
            self.bitbuf >>= 20;
            self.bitsleft -= 20;

            self.state = StreamState::DynamicPrecodeLens {
                num_litlen_syms,
                num_offset_syms,
                num_explicit_precode_lens,
                precode_idx: 1,
            };
        } else if block_type == DEFLATE_BLOCKTYPE_UNCOMPRESSED {
            // Uncompressed block: skip 3 header bits + padding to byte boundary,
            // then read LEN (16-bit LE) and NLEN (16-bit LE).
            //
            // We extract LEN/NLEN directly from bitbuf rather than rewinding
            // input_pos, because staging buffer compaction can reset input_pos
            // to near 0, making rewind impossible.
            //
            // After the 3 header bits, (bitsleft - 3) % 8 gives the padding
            // to the next stream byte boundary.
            let skip = 3 + (self.bitsleft - 3) % 8;
            self.bitbuf >>= skip as u64;
            self.bitsleft -= skip;

            // Now bitbuf is byte-aligned. We need 4 bytes for LEN/NLEN.
            let real_bytes = ((self.bitsleft / 8) as usize).saturating_sub(self.overread_count);
            if real_bytes < 4 {
                return Err(bad.into());
            }

            let len = (self.bitbuf & 0xFFFF) as u16 as usize;
            let nlen = ((self.bitbuf >> 16) & 0xFFFF) as u16;
            self.bitbuf >>= 32;
            self.bitsleft -= 32;

            if len != (!nlen) as usize {
                return Err(bad.into());
            }

            // Any remaining real bytes in bitbuf are the first bytes of the
            // uncompressed data. copy_uncompressed drains them before reading
            // from the staging buffer.
            self.state = StreamState::UncompressedData { remaining: len };
        } else if block_type == DEFLATE_BLOCKTYPE_STATIC_HUFFMAN {
            self.bitbuf >>= 3;
            self.bitsleft -= 3;

            if !self.inner.static_codes_loaded {
                self.inner.static_codes_loaded = true;

                for i in 0..144 {
                    self.inner.lens[i] = 8;
                }
                for i in 144..256 {
                    self.inner.lens[i] = 9;
                }
                for i in 256..280 {
                    self.inner.lens[i] = 7;
                }
                for i in 280..288 {
                    self.inner.lens[i] = 8;
                }
                for i in 288..320 {
                    self.inner.lens[i] = 5;
                }

                if !build_decode_table(
                    &mut self.inner.offset_decode_table,
                    &self.inner.lens[288..],
                    32,
                    &OFFSET_DECODE_RESULTS,
                    OFFSET_TABLEBITS,
                    15,
                    &mut self.inner.sorted_syms,
                    None,
                ) {
                    return Err(bad.into());
                }
                if !build_decode_table(
                    &mut self.inner.litlen_decode_table,
                    &self.inner.lens,
                    288,
                    &LITLEN_DECODE_RESULTS,
                    LITLEN_TABLEBITS,
                    15,
                    &mut self.inner.sorted_syms,
                    Some(&mut self.inner.litlen_tablebits),
                ) {
                    return Err(bad.into());
                }
            }

            self.state = StreamState::CompressedData;
        } else {
            return Err(bad.into());
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Dynamic Huffman: precode lengths
    // -----------------------------------------------------------------------

    fn parse_dynamic_precode_lens(
        &mut self,
        num_litlen_syms: usize,
        num_offset_syms: usize,
        num_explicit_precode_lens: usize,
        mut precode_idx: usize,
    ) -> Result<(), StreamError<S::Error>> {
        let bad = DecompressionError::BadData;
        let input = &self.input_buf[..self.input_len];

        refill_bits(
            &mut self.bitbuf,
            &mut self.bitsleft,
            input,
            &mut self.input_pos,
            &mut self.overread_count,
        )?;

        while precode_idx < num_explicit_precode_lens {
            self.inner.precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[precode_idx] as usize] =
                (self.bitbuf & 7) as u8;
            self.bitbuf >>= 3;
            self.bitsleft -= 3;
            precode_idx += 1;
        }

        for &perm in
            &DEFLATE_PRECODE_LENS_PERMUTATION[num_explicit_precode_lens..DEFLATE_NUM_PRECODE_SYMS]
        {
            self.inner.precode_lens[perm as usize] = 0;
        }

        if !build_decode_table(
            &mut self.inner.precode_decode_table,
            &self.inner.precode_lens,
            DEFLATE_NUM_PRECODE_SYMS,
            &PRECODE_DECODE_RESULTS,
            PRECODE_TABLEBITS,
            DEFLATE_MAX_PRE_CODEWORD_LEN,
            &mut self.inner.sorted_syms,
            None,
        ) {
            return Err(bad.into());
        }

        self.state = StreamState::DynamicCodeLengths {
            num_litlen_syms,
            num_offset_syms,
        };
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Dynamic Huffman: code lengths from precode
    // -----------------------------------------------------------------------

    fn parse_dynamic_code_lengths(
        &mut self,
        num_litlen_syms: usize,
        num_offset_syms: usize,
    ) -> Result<(), StreamError<S::Error>> {
        let bad = DecompressionError::BadData;
        let total_syms = num_litlen_syms + num_offset_syms;
        // We stored lens_idx in self.inner.lens during previous calls — we need
        // to track progress. Use a local that we save back on return.
        // Actually, we re-enter this function from scratch each time via the
        // state machine. The state stores num_litlen_syms and num_offset_syms.
        // We decode all code lengths in one go (they fit in a single staging fill).
        let mut i = 0usize;

        while i < total_syms {
            let input = &self.input_buf[..self.input_len];
            if self.bitsleft < DEFLATE_MAX_PRE_CODEWORD_LEN + 7 {
                refill_bits(
                    &mut self.bitbuf,
                    &mut self.bitsleft,
                    input,
                    &mut self.input_pos,
                    &mut self.overread_count,
                )?;
            }

            let entry = self.inner.precode_decode_table
                [(self.bitbuf & bitmask(DEFLATE_MAX_PRE_CODEWORD_LEN)) as usize];
            self.bitbuf >>= (entry & 0xFF) as u64;
            self.bitsleft -= entry & 0xFF;
            let presym = (entry >> 16) as usize;

            if presym < 16 {
                self.inner.lens[i] = presym as u8;
                i += 1;
                continue;
            }

            if presym == 16 {
                if i == 0 {
                    return Err(bad.into());
                }
                let rep_val = self.inner.lens[i - 1];
                let rep_count = 3 + (self.bitbuf & 3) as usize;
                self.bitbuf >>= 2;
                self.bitsleft -= 2;
                for j in 0..6 {
                    self.inner.lens[i + j] = rep_val;
                }
                i += rep_count;
            } else if presym == 17 {
                let rep_count = 3 + (self.bitbuf & 7) as usize;
                self.bitbuf >>= 3;
                self.bitsleft -= 3;
                for j in 0..10 {
                    self.inner.lens[i + j] = 0;
                }
                i += rep_count;
            } else {
                let rep_count = 11 + (self.bitbuf & bitmask(7)) as usize;
                self.bitbuf >>= 7;
                self.bitsleft -= 7;
                self.inner.lens[i..i + rep_count].fill(0);
                i += rep_count;
            }
        }

        if i != total_syms {
            return Err(bad.into());
        }

        if !build_decode_table(
            &mut self.inner.offset_decode_table,
            &self.inner.lens[num_litlen_syms..],
            num_offset_syms,
            &OFFSET_DECODE_RESULTS,
            OFFSET_TABLEBITS,
            15,
            &mut self.inner.sorted_syms,
            None,
        ) {
            return Err(bad.into());
        }

        if !build_decode_table(
            &mut self.inner.litlen_decode_table,
            &self.inner.lens,
            num_litlen_syms,
            &LITLEN_DECODE_RESULTS,
            LITLEN_TABLEBITS,
            15,
            &mut self.inner.sorted_syms,
            Some(&mut self.inner.litlen_tablebits),
        ) {
            return Err(bad.into());
        }

        self.state = StreamState::CompressedData;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Compressed data decoding (Huffman)
    // -----------------------------------------------------------------------

    fn decompress_block(&mut self) -> Result<(), StreamError<S::Error>> {
        let bad = DecompressionError::BadData;

        // Handle pending match from previous fill()
        if let Some(pm) = self.pending_match.take() {
            let space = self.output_space();
            if space == 0 {
                self.pending_match = Some(pm);
                return Ok(());
            }
            if pm.length > space {
                // Partial write: fill available space, save remainder.
                // Match offset is relative, so the remaining portion
                // references the same lookback distance from the new write_pos.
                self.write_match_inline(pm.offset, space);
                self.pending_match = Some(PendingMatch {
                    offset: pm.offset,
                    length: pm.length - space,
                });
                return Ok(());
            }
            self.write_match_inline(pm.offset, pm.length);
            if self.peek_len() >= self.capacity {
                return Ok(());
            }
        }

        // Handle pending literal from previous fill() (buffer was full)
        if let Some(lit) = self.pending_literal.take() {
            if self.write_pos >= self.buffer.len() {
                self.pending_literal = Some(lit);
                return Ok(());
            }
            self.buffer[self.write_pos] = lit;
            self.write_pos += 1;
            if self.peek_len() >= self.capacity {
                return Ok(());
            }
        }

        let litlen_tablemask = bitmask(self.inner.litlen_tablebits);
        let out_fastloop_end = self.buffer.len().saturating_sub(FASTLOOP_MAX_BYTES_WRITTEN);

        // Outer loop: refills the staging buffer and re-enters the fastloop
        // whenever enough fresh input data is available. Without this, the
        // fastloop would only run once per decompress_block() call (on the
        // initial 512-byte staging buffer fill), leaving all subsequent
        // decompression to the slower generic loop.
        'refill: loop {
            // Refill staging buffer from source
            self.fill_input().map_err(StreamError::Source)?;

            let input = &self.input_buf[..self.input_len];
            let in_fastloop_end = input.len().saturating_sub(FASTLOOP_MAX_BYTES_READ);

            // --- Fastloop ---
            // Extract hot variables into locals for register promotion.
            // The whole-buffer decompressor uses local &mut references which
            // LLVM keeps in registers. Struct field access through &mut self
            // can force loads/stores at function boundaries.
            if self.input_pos < in_fastloop_end && self.write_pos < out_fastloop_end {
                let mut bitbuf = self.bitbuf;
                let mut bitsleft = self.bitsleft;
                let mut in_pos = self.input_pos;
                let mut out_pos = self.write_pos;

                refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                let mut entry =
                    table_lookup(&self.inner.litlen_decode_table, bitbuf & litlen_tablemask);

                // Fastloop exit reason (avoids early returns that skip write-back)
                #[derive(PartialEq)]
                enum Exit {
                    Bounds,
                    EndOfBlock,
                    BadData,
                }

                let exit = 'fastloop: {
                    loop {
                        let mut saved_bitbuf = bitbuf;
                        bitbuf >>= (entry & 0xFF) as u64;
                        bitsleft -= entry & 0xFF;

                        if entry & HUFFDEC_LITERAL != 0 {
                            let lit = (entry >> 16) as u8;
                            entry = table_lookup(
                                &self.inner.litlen_decode_table,
                                bitbuf & litlen_tablemask,
                            );
                            saved_bitbuf = bitbuf;
                            bitbuf >>= (entry & 0xFF) as u64;
                            bitsleft -= entry & 0xFF;
                            self.buffer[out_pos] = lit;
                            out_pos += 1;

                            if entry & HUFFDEC_LITERAL != 0 {
                                let lit = (entry >> 16) as u8;
                                entry = table_lookup(
                                    &self.inner.litlen_decode_table,
                                    bitbuf & litlen_tablemask,
                                );
                                saved_bitbuf = bitbuf;
                                bitbuf >>= (entry & 0xFF) as u64;
                                bitsleft -= entry & 0xFF;
                                self.buffer[out_pos] = lit;
                                out_pos += 1;

                                if entry & HUFFDEC_LITERAL != 0 {
                                    self.buffer[out_pos] = (entry >> 16) as u8;
                                    out_pos += 1;
                                    entry = table_lookup(
                                        &self.inner.litlen_decode_table,
                                        bitbuf & litlen_tablemask,
                                    );
                                    refill_bits_fast(
                                        &mut bitbuf,
                                        &mut bitsleft,
                                        input,
                                        &mut in_pos,
                                    );
                                    if in_pos < in_fastloop_end && out_pos < out_fastloop_end {
                                        continue;
                                    }
                                    break 'fastloop Exit::Bounds;
                                }
                            }
                        }

                        if entry & HUFFDEC_EXCEPTIONAL != 0 {
                            if entry & HUFFDEC_END_OF_BLOCK != 0 {
                                break 'fastloop Exit::EndOfBlock;
                            }
                            entry = table_lookup(
                                &self.inner.litlen_decode_table,
                                (entry >> 16) as u64 + extract_varbits(bitbuf, (entry >> 8) & 0x3F),
                            );
                            saved_bitbuf = bitbuf;
                            bitbuf >>= (entry & 0xFF) as u64;
                            bitsleft -= entry & 0xFF;

                            if entry & HUFFDEC_LITERAL != 0 {
                                self.buffer[out_pos] = (entry >> 16) as u8;
                                out_pos += 1;
                                entry = table_lookup(
                                    &self.inner.litlen_decode_table,
                                    bitbuf & litlen_tablemask,
                                );
                                refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                                if in_pos < in_fastloop_end && out_pos < out_fastloop_end {
                                    continue;
                                }
                                break 'fastloop Exit::Bounds;
                            }
                            if entry & HUFFDEC_END_OF_BLOCK != 0 {
                                break 'fastloop Exit::EndOfBlock;
                            }
                        }

                        // Decode match length
                        let length = (entry >> 16) as usize
                            + (extract_varbits8(saved_bitbuf, entry) >> ((entry >> 8) as u8 as u64))
                                as usize;

                        // Decode match offset
                        let mut oentry = table_lookup(
                            &self.inner.offset_decode_table,
                            bitbuf & bitmask(OFFSET_TABLEBITS),
                        );

                        // Conditional refill: after a multi-literal chain +
                        // length decode, bitsleft may be too low to consume
                        // the full offset entry and preload next litlen.
                        if bitsleft < 28 + self.inner.litlen_tablebits {
                            refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                        }

                        if oentry & HUFFDEC_EXCEPTIONAL != 0 {
                            bitbuf >>= OFFSET_TABLEBITS as u64;
                            bitsleft -= OFFSET_TABLEBITS;
                            oentry = table_lookup(
                                &self.inner.offset_decode_table,
                                (oentry >> 16) as u64
                                    + extract_varbits(bitbuf, (oentry >> 8) & 0x3F),
                            );
                        }
                        let saved_bitbuf_off = bitbuf;
                        bitbuf >>= (oentry & 0xFF) as u64;
                        bitsleft -= oentry & 0xFF;

                        let offset = (oentry >> 16) as usize
                            + (extract_varbits8(saved_bitbuf_off, oentry)
                                >> ((oentry >> 8) as u8 as u64))
                                as usize;

                        if offset == 0 || offset > out_pos {
                            break 'fastloop Exit::BadData;
                        }

                        // Refill BEFORE preload: after a multi-literal + match path,
                        // bitsleft can be < litlen_tablebits, causing the preload to
                        // read stale zero bits. Refilling first ensures enough valid
                        // bits for the table lookup.
                        refill_bits_fast(&mut bitbuf, &mut bitsleft, input, &mut in_pos);
                        entry = table_lookup(
                            &self.inner.litlen_decode_table,
                            bitbuf & litlen_tablemask,
                        );

                        super::fastloop_match_copy(
                            &mut self.buffer,
                            out_pos,
                            out_pos - offset,
                            length,
                            offset,
                        );
                        out_pos += length;

                        if in_pos >= in_fastloop_end || out_pos >= out_fastloop_end {
                            break 'fastloop Exit::Bounds;
                        }
                    }
                };

                // Write back locals
                self.bitbuf = bitbuf;
                self.bitsleft = bitsleft;
                self.input_pos = in_pos;
                self.write_pos = out_pos;

                match exit {
                    Exit::EndOfBlock => {
                        self.finish_block()?;
                        return Ok(());
                    }
                    Exit::BadData => return Err(bad.into()),
                    Exit::Bounds => {}
                }
            }

            // --- Generic loop ---
            // Handles remaining symbols when the fastloop can't run (not enough
            // input runway or output space). Refills the staging buffer as needed
            // and jumps back to the fastloop via 'refill when fresh data arrives.
            loop {
                // Refill staging buffer when running low
                if self.input_len - self.input_pos < 16 {
                    let old_avail = self.input_len - self.input_pos;
                    self.fill_input().map_err(StreamError::Source)?;
                    let new_avail = self.input_len - self.input_pos;

                    // If new data was loaded and we had overread zeros in bitbuf
                    // from the previous staging buffer, purge them.
                    if self.overread_count > 0 && new_avail > old_avail {
                        let real_bits = self.bitsleft - (self.overread_count as u32 * 8);
                        self.bitsleft = real_bits;
                        self.bitbuf &= bitmask(real_bits);
                        self.overread_count = 0;
                    }

                    // Re-enter fastloop if we got enough fresh data and have output space
                    if new_avail >= FASTLOOP_MAX_BYTES_READ + 16
                        && self.write_pos < out_fastloop_end
                    {
                        continue 'refill;
                    }
                }

                let input = &self.input_buf[..self.input_len];
                refill_bits(
                    &mut self.bitbuf,
                    &mut self.bitsleft,
                    input,
                    &mut self.input_pos,
                    &mut self.overread_count,
                )?;

                let mut entry = table_lookup(
                    &self.inner.litlen_decode_table,
                    self.bitbuf & litlen_tablemask,
                );
                let mut saved_bitbuf = self.bitbuf;
                self.bitbuf >>= (entry & 0xFF) as u64;
                self.bitsleft -= entry & 0xFF;

                if entry & HUFFDEC_SUBTABLE_POINTER != 0 {
                    entry = table_lookup(
                        &self.inner.litlen_decode_table,
                        (entry >> 16) as u64 + extract_varbits(self.bitbuf, (entry >> 8) & 0x3F),
                    );
                    saved_bitbuf = self.bitbuf;
                    self.bitbuf >>= (entry & 0xFF) as u64;
                    self.bitsleft -= entry & 0xFF;
                }

                let value = entry >> 16;

                if entry & HUFFDEC_LITERAL != 0 {
                    if self.write_pos >= self.buffer.len() {
                        // Output buffer full — save literal and return to let fill() compact.
                        self.pending_literal = Some(value as u8);
                        return Ok(());
                    }
                    self.buffer[self.write_pos] = value as u8;
                    self.write_pos += 1;
                    if self.peek_len() >= self.capacity {
                        return Ok(());
                    }
                    continue;
                }

                if entry & HUFFDEC_END_OF_BLOCK != 0 {
                    self.finish_block()?;
                    return Ok(());
                }

                // Decode match length
                let length = value as usize
                    + (extract_varbits8(saved_bitbuf, entry) >> ((entry >> 8) as u8 as u64))
                        as usize;

                // Decode match offset
                let mut oentry = table_lookup(
                    &self.inner.offset_decode_table,
                    self.bitbuf & bitmask(OFFSET_TABLEBITS),
                );
                if oentry & HUFFDEC_EXCEPTIONAL != 0 {
                    self.bitbuf >>= OFFSET_TABLEBITS as u64;
                    self.bitsleft -= OFFSET_TABLEBITS;
                    oentry = table_lookup(
                        &self.inner.offset_decode_table,
                        (oentry >> 16) as u64 + extract_varbits(self.bitbuf, (oentry >> 8) & 0x3F),
                    );
                }
                let saved_bitbuf_off = self.bitbuf;
                self.bitbuf >>= (oentry & 0xFF) as u64;
                self.bitsleft -= oentry & 0xFF;

                let offset = (oentry >> 16) as usize
                    + (extract_varbits8(saved_bitbuf_off, oentry) >> ((oentry >> 8) as u8 as u64))
                        as usize;

                if offset == 0 || offset > self.write_pos {
                    return Err(bad.into());
                }

                let space = self.output_space();
                if length > space {
                    if space > 0 {
                        self.write_match_inline(offset, space);
                        self.pending_match = Some(PendingMatch {
                            offset,
                            length: length - space,
                        });
                    } else {
                        self.pending_match = Some(PendingMatch { offset, length });
                    }
                    return Ok(());
                }

                self.write_match_inline(offset, length);

                if self.peek_len() >= self.capacity {
                    return Ok(());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Match writing helpers
    // -----------------------------------------------------------------------

    #[inline(always)]
    fn write_match_inline(&mut self, offset: usize, length: usize) {
        let src_start = self.write_pos - offset;
        if offset >= length {
            self.buffer
                .copy_within(src_start..src_start + length, self.write_pos);
        } else if offset == 1 {
            let byte = self.buffer[src_start];
            self.buffer[self.write_pos..self.write_pos + length].fill(byte);
        } else if length <= 32 {
            for i in 0..length {
                self.buffer[self.write_pos + i] = self.buffer[src_start + i];
            }
        } else {
            self.buffer
                .copy_within(src_start..src_start + offset, self.write_pos);
            let mut copied = offset;
            while copied < length {
                let chunk = copied.min(length - copied);
                self.buffer.copy_within(
                    self.write_pos..self.write_pos + chunk,
                    self.write_pos + copied,
                );
                copied += chunk;
            }
        }
        self.write_pos += length;
    }

    // -----------------------------------------------------------------------
    // Block completion
    // -----------------------------------------------------------------------

    fn finish_block(&mut self) -> Result<(), StreamError<S::Error>> {
        if self.is_final_block {
            // Verify overread count
            let extra_bytes = (self.bitsleft / 8) as usize;
            if self.overread_count > extra_bytes {
                return Err(DecompressionError::BadData.into());
            }
            if self.wrapper == WrapperFormat::Raw {
                self.state = StreamState::Done;
            } else {
                self.state = StreamState::WrapperFooter;
            }
        } else {
            self.state = StreamState::BlockHeader;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Uncompressed block copying
    // -----------------------------------------------------------------------

    fn copy_uncompressed(&mut self, mut remaining: usize) -> Result<(), StreamError<S::Error>> {
        // Phase 1: Drain any real bytes left in bitbuf from header parsing.
        // After parse_block_header extracts LEN/NLEN, the remaining bytes in
        // bitbuf are the first bytes of uncompressed data (overread bytes at
        // the high end are garbage and must not be copied).
        while remaining > 0 && self.output_space() > 0 {
            let real_in_bitbuf = ((self.bitsleft / 8) as usize).saturating_sub(self.overread_count);
            if real_in_bitbuf == 0 {
                break;
            }
            self.buffer[self.write_pos] = (self.bitbuf & 0xFF) as u8;
            self.bitbuf >>= 8;
            self.bitsleft -= 8;
            self.write_pos += 1;
            remaining -= 1;
        }

        // If all real bytes are drained, clear bitbuf entirely
        if (self.bitsleft / 8) as usize <= self.overread_count {
            self.bitbuf = 0;
            self.bitsleft = 0;
            self.overread_count = 0;
        }

        if remaining == 0 {
            self.finish_block()?;
            return Ok(());
        }

        if self.output_space() == 0 {
            self.state = StreamState::UncompressedData { remaining };
            return Ok(());
        }

        // Phase 2: Read from staging buffer (bitbuf is empty now).
        self.ensure_input_bytes(1).map_err(StreamError::Source)?;
        let available = (self.input_len - self.input_pos).min(remaining);
        let can_write = available.min(self.output_space());

        if can_write == 0 {
            if remaining > 0 && self.output_space() == 0 {
                self.state = StreamState::UncompressedData { remaining };
                return Ok(());
            }
            if remaining > 0 {
                return Err(DecompressionError::BadData.into());
            }
        }

        self.buffer[self.write_pos..self.write_pos + can_write]
            .copy_from_slice(&self.input_buf[self.input_pos..self.input_pos + can_write]);
        self.write_pos += can_write;
        self.input_pos += can_write;

        let new_remaining = remaining - can_write;
        if new_remaining == 0 {
            self.finish_block()?;
        } else {
            self.state = StreamState::UncompressedData {
                remaining: new_remaining,
            };
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Wrapper footer parsing
    // -----------------------------------------------------------------------

    fn parse_wrapper_footer(&mut self) -> Result<(), StreamError<S::Error>> {
        let bad = DecompressionError::BadData;

        // Compute checksum for all output produced so far
        self.flush_checksum();

        // Byte-align: discard DEFLATE padding bits (bits after the last
        // code up to the next byte boundary).
        let padding = self.bitsleft % 8;
        self.bitbuf >>= padding as u64;
        self.bitsleft -= padding;

        let footer_size: usize = match self.wrapper {
            WrapperFormat::Zlib => 4,
            WrapperFormat::Gzip => 8,
            WrapperFormat::Raw => {
                self.state = StreamState::Done;
                return Ok(());
            }
        };

        // Collect footer bytes from three sources (in order):
        // 1. Real (non-overread) bytes already in bitbuf
        // 2. Remaining bytes in the staging buffer past input_pos
        // 3. More bytes pulled from the source
        let real_in_bitbuf = (self.bitsleft / 8) as usize - self.overread_count;
        let mut footer = [0u8; 8];
        let mut pos = 0;

        // Source 1: extract real bytes from bitbuf (LSB = earliest in stream)
        let from_bitbuf = real_in_bitbuf.min(footer_size);
        for _ in 0..from_bitbuf {
            footer[pos] = (self.bitbuf & 0xFF) as u8;
            self.bitbuf >>= 8;
            self.bitsleft -= 8;
            pos += 1;
        }

        // Done with bitbuf — clear it
        self.bitbuf = 0;
        self.bitsleft = 0;
        self.overread_count = 0;

        // Source 2 & 3: staging buffer then source
        while pos < footer_size {
            if self.input_pos < self.input_len {
                footer[pos] = self.input_buf[self.input_pos];
                self.input_pos += 1;
                pos += 1;
            } else {
                self.fill_input().map_err(StreamError::Source)?;
                if self.input_pos >= self.input_len {
                    return Err(bad.into());
                }
            }
        }

        // Verify footer
        let matched = match self.wrapper {
            WrapperFormat::Zlib => {
                let expected = u32::from_be_bytes([footer[0], footer[1], footer[2], footer[3]]);
                self.checksum == expected
            }
            WrapperFormat::Gzip => {
                let expected_crc = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
                let expected_size =
                    u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
                self.checksum == expected_crc && (self.total_output as u32) == expected_size
            }
            WrapperFormat::Raw => unreachable!(),
        };

        self.checksum_matched = Some(matched);
        if !matched && !self.skip_checksum {
            return Err(DecompressionError::ChecksumMismatch.into());
        }

        self.state = StreamState::Done;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// std integration: Read + BufRead
// ---------------------------------------------------------------------------

#[cfg(feature = "std")]
impl<R: std::io::BufRead> std::io::BufRead for StreamDecompressor<BufReadSource<R>> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.peek().is_empty() && !self.is_done() {
            self.fill().map_err(|e| match e {
                StreamError::Source(e) => e,
                StreamError::Decompress(e) => {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                }
            })?;
        }
        Ok(self.peek())
    }

    fn consume(&mut self, amt: usize) {
        self.advance(amt);
    }
}

#[cfg(feature = "std")]
impl<R: std::io::BufRead> std::io::Read for StreamDecompressor<BufReadSource<R>> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = std::io::BufRead::fill_buf(self)?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        std::io::BufRead::consume(self, n);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn stream_decompress_all<S: InputSource>(
        dec: &mut StreamDecompressor<S>,
    ) -> Result<Vec<u8>, StreamError<S::Error>>
    where
        S::Error: core::fmt::Debug,
    {
        let mut output = Vec::new();
        while !dec.is_done() {
            dec.fill()?;
            let data = dec.peek();
            output.extend_from_slice(data);
            let n = data.len();
            dec.advance(n);
        }
        Ok(output)
    }

    #[test]
    fn test_stream_deflate_basic() {
        let data = b"Hello, World! Hello, World! Hello, World!";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::deflate(&compressed[..csize], 64 * 1024);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(&output, data);
    }

    #[test]
    fn test_stream_zlib_basic() {
        let data = b"Hello, World! Hello, World! Hello, World!";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::zlib(&compressed[..csize], 64 * 1024);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(&output, data);
    }

    #[test]
    fn test_stream_gzip_basic() {
        let data = b"Hello, World! Hello, World! Hello, World!";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::gzip(&compressed[..csize], 64 * 1024);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(&output, data);
    }

    #[test]
    fn test_stream_all_levels_deflate() {
        let data: Vec<u8> = (0..=255).cycle().take(10_000).collect();
        for level in 0..=12 {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
            let bound = c.deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();

            let mut dec = StreamDecompressor::deflate(&compressed[..csize], 64 * 1024);
            let output = stream_decompress_all(&mut dec).unwrap();
            assert_eq!(output, data, "deflate level {level}");
        }
    }

    #[test]
    fn test_stream_all_formats_all_levels() {
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        for level in 0..=12 {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());

            // DEFLATE
            {
                let bound = c.deflate_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c.deflate_compress(&data, &mut compressed).unwrap();
                let mut dec = StreamDecompressor::deflate(&compressed[..csize], 64 * 1024);
                let output = stream_decompress_all(&mut dec).unwrap();
                assert_eq!(output, data, "streaming deflate level {level}");
            }

            // zlib
            {
                let bound = c.zlib_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c.zlib_compress(&data, &mut compressed).unwrap();
                let mut dec = StreamDecompressor::zlib(&compressed[..csize], 64 * 1024);
                let output = stream_decompress_all(&mut dec).unwrap();
                assert_eq!(output, data, "streaming zlib level {level}");
            }

            // gzip
            {
                let bound = c.gzip_compress_bound(data.len());
                let mut compressed = vec![0u8; bound];
                let csize = c.gzip_compress(&data, &mut compressed).unwrap();
                let mut dec = StreamDecompressor::gzip(&compressed[..csize], 64 * 1024);
                let output = stream_decompress_all(&mut dec).unwrap();
                assert_eq!(output, data, "streaming gzip level {level}");
            }
        }
    }

    #[test]
    fn test_stream_small_capacity() {
        let data: Vec<u8> = (0..=255).cycle().take(10_000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        // Small capacity to exercise compaction and re-entry
        let mut dec = StreamDecompressor::deflate(&compressed[..csize], 512);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(output, data);
    }

    #[test]
    fn test_stream_parity_with_whole_buffer() {
        let data: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        for level in [1, 6, 12] {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
            let bound = c.deflate_compress_bound(data.len());
            let mut compressed = vec![0u8; bound];
            let csize = c.deflate_compress(&data, &mut compressed).unwrap();

            let mut d = Decompressor::new();
            let mut wb_output = vec![0u8; data.len()];
            let wb_size = d
                .deflate_decompress(&compressed[..csize], &mut wb_output, enough::Unstoppable)
                .unwrap()
                .output_written;

            let mut dec = StreamDecompressor::deflate(&compressed[..csize], 4096);
            let stream_output = stream_decompress_all(&mut dec).unwrap();

            assert_eq!(wb_size, stream_output.len(), "level {level}: size mismatch");
            assert_eq!(
                &wb_output[..wb_size],
                &stream_output,
                "level {level}: content mismatch"
            );
        }
    }

    #[test]
    fn test_stream_empty() {
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(1).unwrap());
        let bound = c.deflate_compress_bound(0);
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&[], &mut compressed).unwrap();

        let mut dec = StreamDecompressor::deflate(&compressed[..csize], 1024);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn test_stream_all_zeros() {
        let data = vec![0u8; 100_000];
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::deflate(&compressed[..csize], 8192);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(output.len(), data.len());
        assert_eq!(output, data);
    }

    /// Custom InputSource that yields at most `chunk_size` bytes per fill_buf call.
    struct ChunkedSource<'a> {
        data: &'a [u8],
        chunk_size: usize,
    }

    impl InputSource for ChunkedSource<'_> {
        type Error = core::convert::Infallible;

        fn fill_buf(&mut self) -> Result<&[u8], core::convert::Infallible> {
            let n = self.data.len().min(self.chunk_size);
            Ok(&self.data[..n])
        }

        fn consume(&mut self, n: usize) {
            self.data = &self.data[n..];
        }
    }

    #[test]
    fn test_stream_chunk_stress_1byte() {
        // 1-byte-at-a-time input source — exercises staging buffer refill heavily
        let data: Vec<u8> = (0..=255).cycle().take(5_000).collect();
        for level in [1, 6, 12] {
            let mut c =
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());

            for format in ["deflate", "zlib", "gzip"] {
                let (compressed, csize) = match format {
                    "deflate" => {
                        let bound = c.deflate_compress_bound(data.len());
                        let mut buf = vec![0u8; bound];
                        let n = c.deflate_compress(&data, &mut buf).unwrap();
                        (buf, n)
                    }
                    "zlib" => {
                        let bound = c.zlib_compress_bound(data.len());
                        let mut buf = vec![0u8; bound];
                        let n = c.zlib_compress(&data, &mut buf).unwrap();
                        (buf, n)
                    }
                    "gzip" => {
                        let bound = c.gzip_compress_bound(data.len());
                        let mut buf = vec![0u8; bound];
                        let n = c.gzip_compress(&data, &mut buf).unwrap();
                        (buf, n)
                    }
                    _ => unreachable!(),
                };

                let source = ChunkedSource {
                    data: &compressed[..csize],
                    chunk_size: 1,
                };
                let mut dec = match format {
                    "deflate" => StreamDecompressor::deflate(source, 1024),
                    "zlib" => StreamDecompressor::zlib(source, 1024),
                    "gzip" => StreamDecompressor::gzip(source, 1024),
                    _ => unreachable!(),
                };
                let output = stream_decompress_all(&mut dec).unwrap();
                assert_eq!(output, data, "{format} L{level} 1-byte chunks");
            }
        }
    }

    #[test]
    fn test_stream_chunk_stress_64byte() {
        // 64-byte chunks — typical small BufRead buffer
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(&data, &mut compressed).unwrap();

        let source = ChunkedSource {
            data: &compressed[..csize],
            chunk_size: 64,
        };
        let mut dec = StreamDecompressor::gzip(source, 4096);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(output, data);
    }

    #[test]
    fn test_stream_scanline() {
        // Simulates PNG scanline-at-a-time decompression:
        // fill until we have row_stride bytes, process, advance, repeat.
        let row_width = 640;
        let row_stride = 1 + row_width; // filter byte + pixel data
        let height = 100;
        let data: Vec<u8> = (0..=255).cycle().take(row_stride * height).collect();

        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(&data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::zlib(&compressed[..csize], 2 * row_stride);
        let mut output = Vec::new();

        for _row in 0..height {
            while dec.peek().len() < row_stride {
                dec.fill().unwrap();
            }
            let row_data = &dec.peek()[..row_stride];
            output.extend_from_slice(row_data);
            dec.advance(row_stride);
        }

        assert_eq!(output, data);
    }

    #[test]
    fn test_stream_1mb_mixed() {
        // Exercises multi-block decompression with pseudo-random data that
        // triggers buffer-full compaction in the generic loop.
        let size = 1_000_000;
        let mut data = Vec::with_capacity(size);
        let mut state: u32 = 0xDEAD_BEEF;
        let mut i = 0;
        while i < size {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let byte = (state >> 16) as u8;
            if i % 256 == 0 && i + 32 <= size {
                data.extend(core::iter::repeat_n(byte, 32));
                i += 32;
            } else {
                data.push(byte);
                i += 1;
            }
        }
        data.truncate(size);

        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(&data, &mut compressed).unwrap();

        let mut dec = StreamDecompressor::deflate(&compressed[..csize], 64 * 1024);
        let mut output = Vec::new();
        while !dec.is_done() {
            dec.fill().unwrap();
            let chunk = dec.peek();
            output.extend_from_slice(chunk);
            let n = chunk.len();
            dec.advance(n);
        }
        assert_eq!(output.len(), data.len());
        assert_eq!(output, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_stream_bufreader() {
        let data: Vec<u8> = (0..=255).cycle().take(50_000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(&data, &mut compressed).unwrap();

        let cursor = std::io::Cursor::new(&compressed[..csize]);
        let reader = std::io::BufReader::new(cursor);
        let mut dec = StreamDecompressor::gzip(BufReadSource(reader), 64 * 1024);

        let mut output = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut output).unwrap();
        assert_eq!(output, data);
    }

    // -----------------------------------------------------------------------
    // skip_checksum tests (streaming)
    // -----------------------------------------------------------------------

    /// Corrupt Adler-32 in valid zlib stream: strict mode fails, skip mode succeeds.
    #[test]
    fn stream_zlib_skip_checksum_corrupt_adler() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(&data, &mut compressed).unwrap();

        // Corrupt Adler-32 footer
        compressed[csize - 1] ^= 0xFF;

        // Strict: should fail
        let mut dec = StreamDecompressor::zlib(&compressed[..csize], 64 * 1024);
        let err = stream_decompress_all(&mut dec).unwrap_err();
        assert!(
            matches!(
                err,
                StreamError::Decompress(DecompressionError::ChecksumMismatch)
            ),
            "expected ChecksumMismatch, got: {err:?}"
        );

        // Skip: should succeed with checksum_matched = Some(false)
        let mut dec =
            StreamDecompressor::zlib(&compressed[..csize], 64 * 1024).with_skip_checksum(true);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(output.len(), data.len());
        assert_eq!(output, data);
        assert_eq!(dec.checksum_matched(), Some(false));
    }

    /// Corrupt CRC32 in valid gzip stream: strict mode fails, skip mode succeeds.
    #[test]
    fn stream_gzip_skip_checksum_corrupt_crc() {
        let data: Vec<u8> = (0..=255).cycle().take(5000).collect();
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.gzip_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.gzip_compress(&data, &mut compressed).unwrap();

        // Corrupt CRC32
        compressed[csize - 8] ^= 0xFF;

        // Strict: should fail
        let mut dec = StreamDecompressor::gzip(&compressed[..csize], 64 * 1024);
        let err = stream_decompress_all(&mut dec).unwrap_err();
        assert!(
            matches!(
                err,
                StreamError::Decompress(DecompressionError::ChecksumMismatch)
            ),
            "expected ChecksumMismatch, got: {err:?}"
        );

        // Skip: should succeed
        let mut dec =
            StreamDecompressor::gzip(&compressed[..csize], 64 * 1024).with_skip_checksum(true);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(output.len(), data.len());
        assert_eq!(output, data);
        assert_eq!(dec.checksum_matched(), Some(false));
    }

    /// Valid zlib stream with skip_checksum: checksum_matched() == Some(true).
    #[test]
    fn stream_zlib_skip_checksum_valid_reports_true() {
        let data = b"hello stream skip_checksum test";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.zlib_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.zlib_compress(data, &mut compressed).unwrap();

        let mut dec =
            StreamDecompressor::zlib(&compressed[..csize], 64 * 1024).with_skip_checksum(true);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(&output, data);
        assert_eq!(dec.checksum_matched(), Some(true));
    }

    /// checksum_matched is None for raw DEFLATE streams (no wrapper checksum).
    #[test]
    fn stream_deflate_checksum_matched_is_none() {
        let data = b"raw deflate stream test";
        let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(6).unwrap());
        let bound = c.deflate_compress_bound(data.len());
        let mut compressed = vec![0u8; bound];
        let csize = c.deflate_compress(data, &mut compressed).unwrap();

        let mut dec =
            StreamDecompressor::deflate(&compressed[..csize], 64 * 1024).with_skip_checksum(true);
        let output = stream_decompress_all(&mut dec).unwrap();
        assert_eq!(&output, data);
        assert_eq!(dec.checksum_matched(), None);
    }
}

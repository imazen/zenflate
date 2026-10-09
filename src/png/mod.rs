//! PNG-specific DEFLATE helpers: independent strips for parallel encode and
//! decode.
//!
//! PNG's `iDOT` layout splits one zlib stream into strips of whole image
//! rows. Each strip is compressed without history from earlier strips, so
//! encoders can compress strips concurrently, decoders that understand
//! `iDOT` can inflate them concurrently with [`StripDecoder`], and the
//! concatenation stays one valid zlib stream that any serial decoder
//! (including [`Decompressor`](crate::Decompressor)) reads.
//!
//! An encoder writes [`StripCompressor::zlib_header`], each strip from
//! [`StripCompressor::compress`] in order, then the big-endian Adler-32 of
//! all filtered rows. Per-strip checksums join with
//! [`adler32_combine`](crate::adler32_combine), so each strip's checksum can
//! be computed alongside its compression. The caller owns buffering and
//! scheduling: a strip can be compressed as soon as its rows are filtered,
//! with one `StripCompressor` per worker on any thread pool, and its
//! compressed size can feed back into the filter choice for that strip.
//!
//! A parallel decoder opens each strip with [`StripDecoder::new`] and checks
//! how it ended: every strip but the last must end at a strip boundary, and
//! the last must reach the stream's final block and trailer. Then it joins
//! each strip's [`adler32()`](StripDecoder::adler32) with
//! [`adler32_combine`](crate::adler32_combine) and compares the result with
//! the last strip's [`trailer()`](StripDecoder::trailer). When those checks
//! pass, the strips decoded exactly as a serial decoder reads the stream.
//!
//! ```
//! use zenflate::png::StripCompressor;
//! use zenflate::{CompressionLevel, Decompressor, Unstoppable, adler32, adler32_combine};
//!
//! let strips: [&[u8]; 2] = [&[1u8; 40_000], &[2u8; 40_000]];
//! let mut c = StripCompressor::new(CompressionLevel::png(4));
//! let mut z = c.zlib_header().to_vec();
//! let mut adler = 1;
//! for (k, strip) in strips.iter().enumerate() {
//!     let mut out = vec![0u8; StripCompressor::bound(strip.len())];
//!     let n = c.compress(strip, k + 1 == strips.len(), &mut out, Unstoppable).unwrap();
//!     z.extend_from_slice(&out[..n]);
//!     adler = adler32_combine(adler, adler32(1, strip), strip.len());
//! }
//! z.extend_from_slice(&adler.to_be_bytes());
//!
//! let mut back = vec![0u8; 80_000];
//! Decompressor::new().zlib_decompress(&z, &mut back, Unstoppable).unwrap();
//! assert_eq!(back, strips.concat());
//! ```
//!
//! Decoding the strips independently (each could run on its own thread):
//!
//! ```
//! # use zenflate::png::StripCompressor;
//! # use zenflate::{CompressionLevel, Unstoppable, adler32, adler32_combine};
//! use zenflate::png::StripDecoder;
//! # let strips: [&[u8]; 2] = [&[1u8; 40_000], &[2u8; 40_000]];
//! # let mut c = StripCompressor::new(CompressionLevel::png(4));
//! # let mut z = c.zlib_header().to_vec();
//! # let mut starts = vec![];
//! # for (k, strip) in strips.iter().enumerate() {
//! #     let mut out = vec![0u8; StripCompressor::bound(strip.len())];
//! #     let n = c.compress(strip, k + 1 == strips.len(), &mut out, Unstoppable).unwrap();
//! #     starts.push(z.len());
//! #     z.extend_from_slice(&out[..n]);
//! # }
//! # z.extend_from_slice(&adler32(1, &strips.concat()).to_be_bytes());
//! // `starts[k]` is where strip k's DEFLATE data begins in `z` (PNG's iDOT
//! // chunk records it); the first strip also takes the 2-byte zlib header.
//! let mut adler = 1;
//! let mut trailer = None;
//! for k in 0..strips.len() {
//!     let begin = if k == 0 { 0 } else { starts[k] };
//!     let end = starts.get(k + 1).copied().unwrap_or(z.len());
//!     let mut d = StripDecoder::new(&z[begin..end], k == 0, 1 << 16);
//!     let mut rows = Vec::new();
//!     while !d.is_done() {
//!         let n = {
//!             let got = d.fill().unwrap();
//!             rows.extend_from_slice(got);
//!             got.len()
//!         };
//!         d.advance(n);
//!     }
//!     assert_eq!(rows, strips[k]);
//!     let last = k + 1 == strips.len();
//!     assert_eq!(d.ended_at_strip_boundary(), !last);
//!     adler = adler32_combine(adler, d.adler32(), rows.len());
//!     trailer = d.trailer();
//! }
//! assert_eq!(trailer, Some(adler));
//! ```

use crate::decompress::streaming::{InputSource, StreamDecompressor};
use crate::error::StreamError;
#[cfg(feature = "compress")]
use crate::{CompressionError, CompressionLevel, Compressor};

#[cfg(all(test, feature = "compress"))]
mod tests;

/// Compresses independent strips of one PNG zlib stream.
///
/// Each strip is raw DEFLATE with no zlib header or trailer and no history
/// from earlier strips. Output does not depend on how many strips this
/// compressor has already compressed, so strips can be spread over several
/// `StripCompressor`s (one per worker) and still produce the same bytes.
#[cfg(feature = "compress")]
#[derive(Clone, Debug)]
pub struct StripCompressor {
    inner: Compressor,
    header: [u8; 2],
}

#[cfg(feature = "compress")]
impl StripCompressor {
    /// A strip compressor at `level` (any level, including full-optimal).
    #[must_use]
    pub fn new(level: CompressionLevel) -> Self {
        Self {
            inner: Compressor::new(level),
            header: crate::compress::zlib_header(level),
        }
    }

    /// The 2-byte zlib header that [`Compressor::zlib_compress`] writes at
    /// this level, to put before the first strip.
    #[must_use]
    pub fn zlib_header(&self) -> [u8; 2] {
        self.header
    }

    /// Compress `strip`. With `is_last` false, the output ends byte-aligned
    /// on a block boundary with no final block (compressed strips end with
    /// the zlib full-flush marker `00 00 ff ff`). With `is_last` true, it
    /// ends with the stream's final block. Size `output` with
    /// [`bound`](Self::bound).
    ///
    /// # Errors
    ///
    /// [`CompressionError::InsufficientSpace`] if `output` is too small, or
    /// a stop error from `stop`. The compressor stays usable either way.
    pub fn compress(
        &mut self,
        strip: &[u8],
        is_last: bool,
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        self.inner
            .deflate_compress_segment(strip, is_last, output, stop)
    }

    /// Compress the strip `input[strip_start..]`, letting matches reach back
    /// into `input[..strip_start]`: the image bytes just before the strip
    /// (only the last 32 KiB are used). History can recover matches across
    /// strip boundaries; encoded size still depends on the input and strip
    /// layout. These strips decode only
    /// after the strips before them, so [`StripDecoder`] cannot decode them
    /// on their own: don't use this for files that carry an `iDOT` table.
    ///
    /// The output still depends only on `input` and `strip_start`, never on
    /// which compressor or thread produced the previous strip, so strips can
    /// be compressed concurrently and in any order. Full-optimal levels
    /// (efforts above 30) can't use the history and compress the strip
    /// alone. `is_last`, `output` and errors are as for
    /// [`compress`](Self::compress); size `output` with
    /// [`bound`](Self::bound) of the strip's length.
    ///
    /// # Panics
    ///
    /// Panics if `strip_start > input.len()`.
    pub fn compress_with_history(
        &mut self,
        input: &[u8],
        strip_start: usize,
        is_last: bool,
        output: &mut [u8],
        stop: impl enough::Stop,
    ) -> Result<usize, CompressionError> {
        assert!(strip_start <= input.len(), "strip_start past the input");
        self.inner
            .deflate_compress_segment_after(input, strip_start, is_last, output, stop)
    }

    /// Upper bound on [`compress`](Self::compress) output for a strip of
    /// `strip_len` bytes.
    #[must_use]
    pub fn bound(strip_len: usize) -> usize {
        Compressor::deflate_compress_segment_bound(strip_len)
    }
}

/// Decodes one strip of a PNG zlib stream on its own, as a stream.
///
/// The strip starts with an empty window, so it can be decoded on any
/// thread as soon as its compressed bytes are available. `source` holds the
/// strip's compressed bytes: for the first strip, starting with the zlib
/// header; for the last, ending with the Adler-32 trailer. Read output with
/// [`fill`](Self::fill), [`peek`](Self::peek) and [`advance`](Self::advance),
/// as with [`StreamDecompressor`].
///
/// A strip that cannot decode on its own (it ends mid-block or with
/// leftover bits, or refers back to data before its start) is an error,
/// never a silent mismatch with a serial decode. This decoder does not
/// verify the trailer itself, since a strip sees only part of the stream:
/// combine every strip's [`adler32()`](Self::adler32) and compare with the
/// last strip's [`trailer()`](Self::trailer).
#[derive(Debug)]
pub struct StripDecoder<S> {
    inner: StreamDecompressor<S>,
}

impl<S: InputSource> StripDecoder<S> {
    /// A decoder for one strip. `first_strip` says whether `source` starts
    /// with the zlib header. See [`StreamDecompressor::deflate`] for the
    /// meaning of `capacity`.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn new(source: S, first_strip: bool, capacity: usize) -> Self {
        let inner = if first_strip {
            StreamDecompressor::zlib(source, capacity).with_checksum(crate::ChecksumPolicy::Report)
        } else {
            StreamDecompressor::zlib_continuation(source, capacity)
        };
        Self {
            inner: inner.with_segment_end(true),
        }
    }

    /// Decode more output; see [`StreamDecompressor::fill`].
    ///
    /// # Errors
    ///
    /// A source error, or a decode error, including a strip that cannot be
    /// decoded on its own.
    pub fn fill(&mut self) -> Result<&[u8], StreamError<S::Error>> {
        self.inner.fill()
    }

    /// Decoded output not yet consumed; see [`StreamDecompressor::peek`].
    #[must_use]
    pub fn peek(&self) -> &[u8] {
        self.inner.peek()
    }

    /// Consume `n` bytes of output; see [`StreamDecompressor::advance`].
    pub fn advance(&mut self, n: usize) {
        self.inner.advance(n);
    }

    /// True once the strip has ended, at a strip boundary or at the
    /// stream's trailer.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    /// True when the strip ended at a strip boundary: byte-aligned, on a
    /// block boundary, with no final block. Every strip but the last must
    /// end this way.
    #[must_use]
    pub fn ended_at_strip_boundary(&self) -> bool {
        self.inner.ended_at_segment_boundary()
    }

    /// The stream's Adler-32 trailer, once the last strip has read it.
    /// `None` for every other strip.
    #[must_use]
    pub fn trailer(&self) -> Option<u32> {
        self.inner.footer_checksum()
    }

    /// Adler-32 of this strip's output so far (starting from 1), including
    /// output not yet consumed. Join strips with
    /// [`adler32_combine`](crate::adler32_combine).
    pub fn adler32(&mut self) -> u32 {
        self.inner.running_checksum()
    }

    /// Consume the decoder and return its input source.
    pub fn into_inner(self) -> S {
        self.inner.into_inner()
    }
}

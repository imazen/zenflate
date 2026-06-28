//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! Built on techniques from [libdeflate](https://github.com/ebiggers/libdeflate),
//! [Zopfli](https://github.com/google/zopfli), and
//! [Brotli](https://github.com/google/brotli).
//!
//! - **Compression** ([`Compressor`]) — buffer-to-buffer. Effort 0-30 with named
//!   presets ([`CompressionLevel::balanced()`], etc.). Parallel gzip via
//!   [`Compressor::gzip_compress_parallel()`].
//! - **Decompression** ([`Decompressor`]) — buffer-to-buffer, fastest mode.
//! - **Streaming decompression** ([`StreamDecompressor`]) — pull-based, works
//!   with any [`InputSource`] including `&[u8]` (zero-cost) and
//!   [`BufReadSource`] for `std::io::BufRead`.
//!
//! All three DEFLATE-based formats (raw DEFLATE, zlib, gzip) are supported for
//! both compression and decompression.
//!
//! # Quick start
//!
//! One-shot helpers compress or decompress a whole buffer in a single call,
//! returning a right-sized `Vec` (require the `alloc` feature):
//!
//! ```
//! use zenflate::CompressionLevel;
//!
//! let data: &[u8] = b"Hello, World! Hello, World! Hello, World!";
//!
//! // Compress to gzip with the balanced preset (`zlib_*` variants share this shape).
//! let compressed = zenflate::gzip_compress(data, CompressionLevel::balanced()).unwrap();
//!
//! // Decompress, capping output at 1 MiB to bound untrusted input.
//! let restored = zenflate::gzip_decompress(&compressed, 1024 * 1024).unwrap();
//! assert_eq!(restored, data);
//! ```
//!
//! For incremental input, compressing into a caller-owned buffer, raw DEFLATE,
//! streaming, parallel gzip, or cancellation, drive [`Compressor`] /
//! [`Decompressor`] directly:
//!
//! ```
//! use zenflate::{Compressor, CompressionLevel, Decompressor, Unstoppable};
//!
//! let data = b"Hello, World! Hello, World! Hello, World!";
//!
//! // Compress (effort 15 = lazy matching, a good default)
//! let mut compressor = Compressor::new(CompressionLevel::balanced());
//! let bound = Compressor::deflate_compress_bound(data.len());
//! let mut compressed = vec![0u8; bound];
//! let csize = compressor.deflate_compress(data, &mut compressed, Unstoppable).unwrap();
//!
//! // Decompress
//! let mut decompressor = Decompressor::new();
//! let mut output = vec![0u8; data.len()];
//! let result = decompressor
//!     .deflate_decompress(&compressed[..csize], &mut output, Unstoppable)
//!     .unwrap();
//! assert_eq!(&output[..result.output_written], &data[..]);
//! ```
//!
//! # Compression levels
//!
//! Use named presets or dial in a specific effort from 0 to 30:
//!
//! | Preset | Effort | Strategy |
//! |--------|--------|----------|
//! | [`CompressionLevel::none()`] | 0 | Store (no compression) |
//! | [`CompressionLevel::fastest()`] | 1 | Turbo hash table |
//! | [`CompressionLevel::fast()`] | 10 | Greedy hash chains |
//! | [`CompressionLevel::balanced()`] | 15 | Lazy matching (default) |
//! | [`CompressionLevel::high()`] | 22 | Double-lazy matching |
//! | [`CompressionLevel::best()`] | 30 | Near-optimal parsing |
//!
//! [`CompressionLevel::new(n)`](CompressionLevel::new) accepts any effort 0-30
//! for fine-grained control between presets. Higher effort within a strategy
//! increases search depth and match quality.
//!
//! [`CompressionLevel::libdeflate(n)`](CompressionLevel::libdeflate) (0-12)
//! produces byte-identical output with C libdeflate.

#![cfg_attr(not(feature = "unchecked"), forbid(unsafe_code))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub(crate) mod constants;
pub mod error;

pub(crate) mod fast_bytes;

pub mod checksum;
#[cfg(feature = "alloc")]
pub mod compress;
pub mod decompress;
#[cfg(feature = "alloc")]
pub(crate) mod matchfinder;

pub use checksum::{Adler32Hasher, Crc32Hasher, adler32, adler32_combine, crc32, crc32_combine};
#[cfg(feature = "alloc")]
pub use compress::{CompressionLevel, Compressor, CompressorSnapshot};
#[cfg(all(feature = "alloc", feature = "std"))]
pub use decompress::streaming::BufReadSource;
#[cfg(feature = "alloc")]
pub use decompress::streaming::{DEFAULT_CAPACITY, InputSource, StreamDecompressor};
pub use decompress::{DecompressOutcome, Decompressor};
pub use enough::{Stop, StopReason, Unstoppable};
#[cfg(feature = "alloc")]
pub use error::StreamError;
pub use error::{CompressionError, DecompressionError};

// ---------------------------------------------------------------------------
// One-shot convenience functions
// ---------------------------------------------------------------------------
//
// Additive wrappers over [`Compressor`] / [`Decompressor`] for the common
// "compress/decompress this whole buffer" job, returning a right-sized `Vec`.
// The builder types remain the power API for incremental, in-place, streaming,
// parallel, or cancellable work. These require the `alloc` feature (they
// allocate the output `Vec`).

#[cfg(all(feature = "alloc", not(feature = "std")))]
use alloc::{vec, vec::Vec};

/// Compress a whole buffer into a gzip stream in one call.
///
/// Wraps [`Compressor::gzip_compress`], sizing the output via
/// [`Compressor::gzip_compress_bound`] and returning a right-sized `Vec`.
/// `level` selects the effort/strategy — [`CompressionLevel::balanced()`] is a
/// good default; see [`CompressionLevel`] for the full 0–30 range and presets.
///
/// For incremental input, compressing into a caller-owned buffer, cancellation,
/// or parallel gzip, drive [`Compressor`] directly.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// let original: &[u8] = b"the quick brown fox jumps over the lazy dog, again and again";
///
/// let compressed = zenflate::gzip_compress(original, CompressionLevel::balanced()).unwrap();
/// let restored = zenflate::gzip_decompress(&compressed, 1024 * 1024).unwrap();
///
/// assert_eq!(restored, original);
/// ```
#[cfg(feature = "alloc")]
pub fn gzip_compress(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>, CompressionError> {
    let mut compressor = Compressor::new(level);
    let mut out = vec![0u8; Compressor::gzip_compress_bound(data.len())];
    let n = compressor.gzip_compress(data, &mut out, Unstoppable)?;
    out.truncate(n);
    Ok(out)
}

/// Decompress a whole gzip stream into a new `Vec` in one call.
///
/// Wraps [`Decompressor::gzip_decompress`]. Because a gzip trailer is
/// attacker-controlled, the decompressed length isn't known up front, so
/// `max_output_size` is a **hard ceiling**: if the data would expand past it,
/// decompression returns [`DecompressionError::OutputLimitExceeded`] instead of
/// allocating without bound. This mirrors the whole-buffer "the buffer you pass
/// is your cap" model and defends against decompression bombs. The returned
/// `Vec` grows only as needed, never beyond the ceiling.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// let original: &[u8] = b"hello hello hello hello world world world";
/// let compressed = zenflate::gzip_compress(original, CompressionLevel::fast()).unwrap();
///
/// let restored = zenflate::gzip_decompress(&compressed, 64 * 1024).unwrap();
/// assert_eq!(restored, original);
/// ```
#[cfg(feature = "alloc")]
pub fn gzip_decompress(data: &[u8], max_output_size: usize) -> Result<Vec<u8>, DecompressionError> {
    decompress_oneshot(data, max_output_size, |d, input, out| {
        d.gzip_decompress(input, out, Unstoppable)
    })
}

/// Compress a whole buffer into a zlib stream in one call.
///
/// Wraps [`Compressor::zlib_compress`], sizing the output via
/// [`Compressor::zlib_compress_bound`] and returning a right-sized `Vec`.
/// `level` selects the effort/strategy — see [`CompressionLevel`].
///
/// For incremental input, compressing into a caller-owned buffer, or
/// cancellation, drive [`Compressor`] directly.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// let original: &[u8] = b"the quick brown fox jumps over the lazy dog, again and again";
///
/// let compressed = zenflate::zlib_compress(original, CompressionLevel::balanced()).unwrap();
/// let restored = zenflate::zlib_decompress(&compressed, 1024 * 1024).unwrap();
///
/// assert_eq!(restored, original);
/// ```
#[cfg(feature = "alloc")]
pub fn zlib_compress(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>, CompressionError> {
    let mut compressor = Compressor::new(level);
    let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
    let n = compressor.zlib_compress(data, &mut out, Unstoppable)?;
    out.truncate(n);
    Ok(out)
}

/// Decompress a whole zlib stream into a new `Vec` in one call.
///
/// Wraps [`Decompressor::zlib_decompress`]. `max_output_size` is a **hard
/// ceiling** on the decompressed length — if the data would expand past it,
/// decompression returns [`DecompressionError::OutputLimitExceeded`] instead of
/// allocating without bound, guarding against decompression bombs. The returned
/// `Vec` grows only as needed, never beyond the ceiling.
///
/// ```
/// use zenflate::CompressionLevel;
///
/// let original: &[u8] = b"hello hello hello hello world world world";
/// let compressed = zenflate::zlib_compress(original, CompressionLevel::fast()).unwrap();
///
/// let restored = zenflate::zlib_decompress(&compressed, 64 * 1024).unwrap();
/// assert_eq!(restored, original);
/// ```
#[cfg(feature = "alloc")]
pub fn zlib_decompress(data: &[u8], max_output_size: usize) -> Result<Vec<u8>, DecompressionError> {
    decompress_oneshot(data, max_output_size, |d, input, out| {
        d.zlib_decompress(input, out, Unstoppable)
    })
}

/// Shared grow-and-retry driver for the one-shot decompressors.
///
/// Starts from a small buffer (so a generous ceiling doesn't force a large
/// up-front allocation), doubles on
/// [`InsufficientSpace`](DecompressionError::InsufficientSpace), and never grows
/// past `max_output_size`. If the data would exceed the ceiling, returns
/// [`OutputLimitExceeded`](DecompressionError::OutputLimitExceeded).
#[cfg(feature = "alloc")]
fn decompress_oneshot(
    data: &[u8],
    max_output_size: usize,
    decode: impl Fn(
        &mut Decompressor,
        &[u8],
        &mut [u8],
    ) -> Result<DecompressOutcome, DecompressionError>,
) -> Result<Vec<u8>, DecompressionError> {
    let mut decompressor = Decompressor::new().with_max_output_size(Some(max_output_size));
    // Heuristic start: ~4x the input (decompression expands), but never larger
    // than the ceiling.
    let mut cap = max_output_size.min(data.len().saturating_mul(4).max(64));
    let mut out = vec![0u8; cap];
    loop {
        match decode(&mut decompressor, data, &mut out) {
            Ok(outcome) => {
                out.truncate(outcome.output_written);
                return Ok(out);
            }
            Err(DecompressionError::InsufficientSpace) => {
                if cap >= max_output_size {
                    // Filled the whole ceiling and the stream still wants more.
                    return Err(DecompressionError::OutputLimitExceeded);
                }
                cap = cap.saturating_mul(2).min(max_output_size).max(cap + 1);
                out.resize(cap, 0);
            }
            Err(e) => return Err(e),
        }
    }
}

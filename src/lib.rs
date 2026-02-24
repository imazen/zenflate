//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! A port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust.
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

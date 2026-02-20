//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! A port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust.
//! Provides **buffer-to-buffer** (non-streaming) compression and decompression for
//! the DEFLATE, zlib, and gzip formats. The entire input and output must fit in
//! memory — there are no `Read`/`Write` adapters. This design enables the
//! aggressive optimizations that make zenflate 2x faster than flate2/miniz_oxide.
//!
//! For streaming use cases, consider using zenflate as a backend inside a
//! buffering wrapper, or use `flate2` directly.
//!
//! # Quick start
//!
//! ```
//! use zenflate::{Compressor, CompressionLevel, Decompressor};
//!
//! let data = b"Hello, World! Hello, World! Hello, World!";
//!
//! // Compress
//! let mut compressor = Compressor::new(CompressionLevel::balanced());
//! let bound = Compressor::deflate_compress_bound(data.len());
//! let mut compressed = vec![0u8; bound];
//! let csize = compressor.deflate_compress(data, &mut compressed).unwrap();
//!
//! // Decompress
//! let mut decompressor = Decompressor::new();
//! let mut output = vec![0u8; data.len()];
//! let dsize = decompressor
//!     .deflate_decompress(&compressed[..csize], &mut output)
//!     .unwrap();
//! assert_eq!(&output[..dsize], &data[..]);
//! ```

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

pub use checksum::{adler32, crc32};
#[cfg(feature = "alloc")]
pub use compress::{CompressionLevel, Compressor};
pub use decompress::Decompressor;
#[cfg(all(feature = "alloc", feature = "std"))]
pub use decompress::streaming::BufReadSource;
#[cfg(feature = "alloc")]
pub use decompress::streaming::{InputSource, StreamDecompressor};
pub use enough::{Stop, StopReason, Unstoppable};
#[cfg(feature = "alloc")]
pub use error::StreamError;
pub use error::{CompressionError, DecompressionError};

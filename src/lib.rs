//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! A port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust.
//! Provides non-streaming, buffer-to-buffer compression and decompression for
//! the DEFLATE, zlib, and gzip formats.
//!
//! # Quick start
//!
//! ```
//! use zenflate::{Compressor, CompressionLevel, Decompressor};
//!
//! let data = b"Hello, World! Hello, World! Hello, World!";
//!
//! // Compress
//! let mut compressor = Compressor::new(CompressionLevel::DEFAULT);
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

pub mod constants;
pub mod error;

pub(crate) mod fast_bytes;

pub mod checksum;
#[cfg(feature = "alloc")]
pub mod compress;
pub mod decompress;
#[cfg(feature = "alloc")]
pub(crate) mod matchfinder;

pub use checksum::{adler32, crc32};
#[cfg(feature = "std")]
pub use compress::gzip_compress_parallel;
#[cfg(feature = "alloc")]
pub use compress::{CompressionLevel, Compressor};
pub use decompress::Decompressor;
pub use error::{CompressionError, DecompressionError};

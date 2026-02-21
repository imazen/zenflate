//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! A port of [libdeflate](https://github.com/ebiggers/libdeflate) to safe Rust.
//!
//! Two decompression modes:
//!
//! - **Whole-buffer** ([`Decompressor`]) — fastest, requires input and output in memory.
//! - **Streaming** ([`StreamDecompressor`]) — pull-based, works with any [`InputSource`]
//!   including `&[u8]` (zero-cost) and [`BufReadSource`] for `std::io::BufRead`.
//!   Implements `Read` + `BufRead` when wrapping a `BufRead` source.
//!
//! Compression is buffer-to-buffer only ([`Compressor`]).
//!
//! # Quick start
//!
//! ```
//! use zenflate::{Compressor, CompressionLevel, Decompressor, Unstoppable};
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
//! let result = decompressor
//!     .deflate_decompress(&compressed[..csize], &mut output, Unstoppable)
//!     .unwrap();
//! assert_eq!(&output[..result.output_written], &data[..]);
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
#[cfg(all(feature = "alloc", feature = "std"))]
pub use decompress::streaming::BufReadSource;
#[cfg(feature = "alloc")]
pub use decompress::streaming::{DEFAULT_CAPACITY, InputSource, StreamDecompressor};
pub use decompress::{DecompressOutcome, Decompressor};
pub use enough::{Stop, StopReason, Unstoppable};
#[cfg(feature = "alloc")]
pub use error::StreamError;
pub use error::{CompressionError, DecompressionError};

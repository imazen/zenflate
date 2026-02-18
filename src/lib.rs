//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! A port of libdeflate to safe Rust. Provides non-streaming, buffer-to-buffer
//! compression and decompression for the DEFLATE, zlib, and gzip formats.

#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod constants;
pub mod error;

pub mod checksum;

pub use checksum::{adler32, crc32};
pub use error::{CompressionError, DecompressionError};

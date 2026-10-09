//! zenflate: Pure Rust DEFLATE/zlib/gzip compression and decompression.
//!
//! Built on techniques from [libdeflate](https://github.com/ebiggers/libdeflate),
//! [Zopfli](https://github.com/google/zopfli), and
//! [Brotli](https://github.com/google/brotli).
//!
//! - **Compression** ([`Compressor`][Compressor]) — buffer-to-buffer. Effort 0-200 with named
//!   presets ([`CompressionLevel::balanced()`][CompressionLevel-balanced], etc.). Parallel gzip via
//!   [`Compressor::gzip_compress_parallel()`][Compressor-gzip_compress_parallel].
//! - **Decompression** ([`Decompressor`]) — buffer-to-buffer, fastest mode.
//! - **Streaming decompression** ([`StreamDecompressor`][StreamDecompressor]) — pull-based, works
//!   with any [`InputSource`][InputSource] including `&[u8]` (zero-cost) and
//!   [`BufReadSource`][BufReadSource] for `std::io::BufRead`.
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
//! Use named presets or dial in a specific effort from 0 to 200:
//!
//! | Preset | Effort | Strategy |
//! |--------|--------|----------|
//! | [`CompressionLevel::none()`][CompressionLevel-none] | 0 | Store (no compression) |
//! | [`CompressionLevel::fastest()`][CompressionLevel-fastest] | 1 | Turbo hash table |
//! | [`CompressionLevel::fast()`][CompressionLevel-fast] | 10 | Greedy hash chains |
//! | [`CompressionLevel::balanced()`][CompressionLevel-balanced] | 15 | Lazy matching (default) |
//! | [`CompressionLevel::high()`][CompressionLevel-high] | 22 | Double-lazy matching |
//! | [`CompressionLevel::best()`][CompressionLevel-best] | 30 | Near-optimal parsing |
//!
//! [`CompressionLevel::new(n)`][CompressionLevel-new] accepts any effort 0-200
//! for fine-grained control between presets. Higher effort within a strategy
//! increases search depth and match quality. Efforts 31-200 engage the
//! Zopfli-style full-optimal parser (`iterations = effort − 16`) — very slow,
//! maximum density.
//!
//! [`CompressionLevel::libdeflate(n)`][CompressionLevel-libdeflate] (0-12)
//! produces byte-identical output with C libdeflate.
//!
//! # Feature flags
//!
//! | Feature | Default | Effect |
//! |---------|---------|--------|
//! | `std` | yes | `std::io::{Read, BufRead}` integration (`BufReadSource`) |
//! | `alloc` | yes (via `std`) | Streaming decompression |
//! | `compress` | yes | [`Compressor`][Compressor] / [`CompressionLevel`][CompressionLevel] (implies `alloc`) |
//! | `simd` | yes | Runtime-dispatched SIMD checksums + matchfinder multiversioning |
//! | `avx512` | yes | AVX-512 SIMD tiers (implies `simd`) |
//! | `threads` | yes | [`Compressor::gzip_compress_parallel()`][Compressor-gzip_compress_parallel] (implies `compress`) |
//! | `unchecked` | no | Elide bounds checks in compression hot paths |
//!
//! Buffer-to-buffer decompression and both checksums are always available,
//! including `no_std` without `alloc`; the error types implement
//! [`core::error::Error`] unconditionally. For a minimal, fast-to-compile
//! decoder use `default-features = false, features = ["std"]` — one
//! dependency, no proc macros, scalar checksums.

#![cfg_attr(
    feature = "compress",
    doc = r"

[Compressor]: crate::Compressor
[CompressionLevel]: crate::CompressionLevel
[CompressionLevel-none]: crate::CompressionLevel::none
[CompressionLevel-fastest]: crate::CompressionLevel::fastest
[CompressionLevel-fast]: crate::CompressionLevel::fast
[CompressionLevel-balanced]: crate::CompressionLevel::balanced
[CompressionLevel-high]: crate::CompressionLevel::high
[CompressionLevel-best]: crate::CompressionLevel::best
[CompressionLevel-new]: crate::CompressionLevel::new
[CompressionLevel-libdeflate]: crate::CompressionLevel::libdeflate
"
)]
#![cfg_attr(
    not(feature = "compress"),
    doc = r"

[Compressor]: https://docs.rs/zenflate/latest/zenflate/struct.Compressor.html
[CompressionLevel]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html
[CompressionLevel-none]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.none
[CompressionLevel-fastest]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.fastest
[CompressionLevel-fast]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.fast
[CompressionLevel-balanced]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.balanced
[CompressionLevel-high]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.high
[CompressionLevel-best]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.best
[CompressionLevel-new]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.new
[CompressionLevel-libdeflate]: https://docs.rs/zenflate/latest/zenflate/struct.CompressionLevel.html#method.libdeflate
"
)]
#![cfg_attr(
    feature = "alloc",
    doc = r"

[StreamDecompressor]: crate::StreamDecompressor
[InputSource]: crate::InputSource
"
)]
#![cfg_attr(
    not(feature = "alloc"),
    doc = r"

[StreamDecompressor]: https://docs.rs/zenflate/latest/zenflate/struct.StreamDecompressor.html
[InputSource]: https://docs.rs/zenflate/latest/zenflate/trait.InputSource.html
"
)]
#![cfg_attr(
    feature = "std",
    doc = r"

[BufReadSource]: crate::BufReadSource
"
)]
#![cfg_attr(
    not(feature = "std"),
    doc = r"

[BufReadSource]: https://docs.rs/zenflate/latest/zenflate/struct.BufReadSource.html
"
)]
#![cfg_attr(
    feature = "threads",
    doc = r"

[Compressor-gzip_compress_parallel]: crate::Compressor::gzip_compress_parallel
"
)]
#![cfg_attr(
    not(feature = "threads"),
    doc = r"

[Compressor-gzip_compress_parallel]: https://docs.rs/zenflate/latest/zenflate/struct.Compressor.html#method.gzip_compress_parallel
"
)]
#![cfg_attr(not(feature = "unchecked"), forbid(unsafe_code))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub(crate) mod constants;
pub mod error;

pub(crate) mod fast_bytes;

#[cfg(all(test, feature = "std", feature = "compress"))]
mod miri_tests;

pub mod checksum;
#[cfg(feature = "compress")]
pub mod compress;
pub mod decompress;
#[cfg(feature = "compress")]
pub(crate) mod matchfinder;
#[cfg(feature = "alloc")]
pub mod png;

pub use checksum::{Adler32Hasher, Crc32Hasher, adler32, adler32_combine, crc32, crc32_combine};
#[cfg(feature = "compress")]
pub use compress::{CompressionLevel, Compressor, CompressorSnapshot};
#[cfg(all(feature = "alloc", feature = "std"))]
pub use decompress::streaming::BufReadSource;
#[cfg(feature = "alloc")]
pub use decompress::streaming::{DEFAULT_CAPACITY, InputSource, StreamDecompressor};
pub use decompress::{ChecksumPolicy, DecompressOutcome, Decompressor};
pub use enough::{Stop, StopReason, Unstoppable};
#[cfg(feature = "alloc")]
pub use error::StreamError;
pub use error::{CompressionError, DecompressionError};

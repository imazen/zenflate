//! Checksum algorithms: Adler-32 (zlib) and CRC-32 (gzip).

mod adler32;
mod crc32;
pub(crate) mod tables;

pub use adler32::adler32;
#[allow(unused_imports)] // Used by future zlib_compress_parallel
pub(crate) use adler32::adler32_combine;
pub use crc32::crc32;
pub(crate) use crc32::crc32_combine;

//! Checksum algorithms: Adler-32 (zlib) and CRC-32 (gzip).

mod adler32;
mod crc32;
pub(crate) mod tables;

pub use adler32::{Adler32Hasher, adler32, adler32_combine};
pub use crc32::{Crc32Hasher, crc32, crc32_combine};

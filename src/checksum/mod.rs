//! Checksum algorithms: Adler-32 (zlib) and CRC-32 (gzip).

mod adler32;
mod crc32;
pub(crate) mod tables;

pub use adler32::adler32;
pub use crc32::crc32;

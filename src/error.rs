//! Error types for compression and decompression.

/// Error returned when compression fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionError {
    /// The output buffer is too small to hold the compressed data.
    InsufficientSpace,
}

/// Error returned when decompression fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecompressionError {
    /// The compressed data is invalid or corrupt.
    BadData,
    /// The output buffer is too small for the decompressed data.
    InsufficientSpace,
}

impl core::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InsufficientSpace => write!(f, "output buffer too small for compressed data"),
        }
    }
}

impl core::fmt::Display for DecompressionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadData => write!(f, "invalid or corrupt compressed data"),
            Self::InsufficientSpace => write!(f, "output buffer too small for decompressed data"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CompressionError {}

#[cfg(feature = "std")]
impl std::error::Error for DecompressionError {}

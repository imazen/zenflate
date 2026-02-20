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

/// Error from streaming decompression: either a decompression error or a source I/O error.
///
/// When `S: InputSource` has `Error = Infallible` (e.g., `&[u8]`), the `Source` variant
/// is uninhabited and the compiler eliminates it entirely.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub enum StreamError<E> {
    /// The compressed data is invalid or corrupt.
    Decompress(DecompressionError),
    /// An error occurred reading from the input source.
    Source(E),
}

#[cfg(feature = "alloc")]
impl<E: core::fmt::Display> core::fmt::Display for StreamError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decompress(e) => write!(f, "{e}"),
            Self::Source(e) => write!(f, "source error: {e}"),
        }
    }
}

#[cfg(feature = "alloc")]
impl<E> From<DecompressionError> for StreamError<E> {
    fn from(e: DecompressionError) -> Self {
        Self::Decompress(e)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CompressionError {}

#[cfg(feature = "std")]
impl std::error::Error for DecompressionError {}

#[cfg(feature = "std")]
impl<E: std::error::Error + 'static> std::error::Error for StreamError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decompress(e) => Some(e),
            Self::Source(e) => Some(e),
        }
    }
}

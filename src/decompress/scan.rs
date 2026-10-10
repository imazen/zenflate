//! Count-only scan of a DEFLATE stream: where it ends, without producing output.

use super::{Decompressor, ZLIB_CINFO_32K_WINDOW, ZLIB_CM_DEFLATE, ZLIB_FOOTER_SIZE};
use crate::error::DecompressionError;

/// Result of [`deflate_scan`] / [`zlib_scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ScanOutcome {
    /// Input bytes the decoder reads: through the end of the stream (zlib: through the
    /// Adler-32 trailer), or, when [`stopped`](Self::stopped), through the last byte
    /// holding bits of the symbol that produced output byte `stop_at`.
    pub input_consumed: usize,
    /// Decompressed bytes counted. When stopped, at least `stop_at` (a match may end
    /// past it).
    pub output_len: usize,
    /// The scan reached `stop_at` output bytes before the stream ended.
    pub stopped: bool,
}

/// Why a scan failed, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanError {
    /// The error the decoder reports for the same input.
    pub error: DecompressionError,
    /// Input bytes loaded when the error was detected: the bad data ends at or before
    /// this position (the bit buffer reads ahead by at most 8 bytes). `input.len()` for
    /// truncated input.
    pub input_pos: usize,
    /// Decompressed bytes produced before the error: what a streaming decoder
    /// delivers before it fails.
    pub output_len: usize,
}

/// Walk a raw DEFLATE stream without writing output: matches are counted, not copied,
/// so there is no output buffer and no window, and the work is linear in the input.
///
/// It runs the decoder's own inflate loop, so it accepts, rejects and consumes exactly
/// what [`Decompressor::deflate_decompress`] does. With `stop_at`, it returns as soon
/// as that many output bytes exist (`stopped`), with the input a streaming decoder
/// must have read to produce them.
///
/// ```
/// use zenflate::{Compressor, CompressionLevel, Unstoppable, deflate_scan};
///
/// let data = vec![7u8; 100_000];
/// let mut c = Compressor::new(CompressionLevel::balanced());
/// let mut z = vec![0u8; Compressor::deflate_compress_bound(data.len())];
/// let n = c.deflate_compress(&data, &mut z, Unstoppable).unwrap();
/// z.truncate(n);
/// z.extend_from_slice(b"trailing");
///
/// let end = deflate_scan(&z, None, Unstoppable).unwrap();
/// assert_eq!((end.input_consumed, end.output_len, end.stopped), (n, 100_000, false));
/// let mid = deflate_scan(&z, Some(10), Unstoppable).unwrap();
/// assert!(mid.stopped && mid.input_consumed <= n);
/// ```
pub fn deflate_scan(
    input: &[u8],
    stop_at: Option<usize>,
    stop: impl enough::Stop,
) -> Result<ScanOutcome, ScanError> {
    let stop_at = stop_at.unwrap_or(usize::MAX);
    if stop_at == 0 {
        return Ok(ScanOutcome {
            input_consumed: 0,
            output_len: 0,
            stopped: true,
        });
    }
    let mut fail_at = (input.len(), 0);
    let mut d = Decompressor::new();
    match d.deflate_decompress_core_impl::<true>(input, &mut [], stop_at, &mut fail_at, &stop) {
        Ok((input_consumed, output_len, stopped)) => Ok(ScanOutcome {
            input_consumed,
            output_len,
            stopped,
        }),
        Err(error) => Err(ScanError {
            error,
            input_pos: fail_at.0,
            output_len: fail_at.1,
        }),
    }
}

/// [`deflate_scan`] for a zlib stream: checks the header as
/// [`Decompressor::zlib_decompress`] does, and counts the 2-byte header and, at the
/// end, the 4-byte Adler-32 trailer (read, not verified).
pub fn zlib_scan(
    input: &[u8],
    stop_at: Option<usize>,
    stop: impl enough::Stop,
) -> Result<ScanOutcome, ScanError> {
    let bad = |input_pos| ScanError {
        error: DecompressionError::InvalidHeader,
        input_pos,
        output_len: 0,
    };
    let [cmf, flg] = *input.first_chunk::<2>().ok_or(bad(input.len()))?;
    if !u16::from_be_bytes([cmf, flg]).is_multiple_of(31)
        || cmf & 0xF != ZLIB_CM_DEFLATE
        || cmf >> 4 > ZLIB_CINFO_32K_WINDOW
        || (flg >> 5) & 1 != 0
    {
        return Err(bad(2));
    }
    let shift = |mut e: ScanError| {
        e.input_pos += 2;
        e
    };
    let mut r = deflate_scan(&input[2..], stop_at, stop).map_err(shift)?;
    r.input_consumed += 2;
    if !r.stopped {
        if input.len() - r.input_consumed < ZLIB_FOOTER_SIZE {
            return Err(ScanError {
                error: DecompressionError::BadData,
                input_pos: input.len(),
                output_len: r.output_len,
            });
        }
        r.input_consumed += ZLIB_FOOTER_SIZE;
    }
    Ok(r)
}

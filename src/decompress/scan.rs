//! Count-only scan of a DEFLATE stream: where it ends, without producing output.

use super::{
    DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN, DEFLATE_BLOCKTYPE_STATIC_HUFFMAN,
    DEFLATE_BLOCKTYPE_UNCOMPRESSED, DEFLATE_MAX_PRE_CODEWORD_LEN, DEFLATE_NUM_PRECODE_SYMS,
    DEFLATE_PRECODE_LENS_PERMUTATION, Decompressor, HUFFDEC_END_OF_BLOCK, HUFFDEC_EXCEPTIONAL,
    HUFFDEC_LITERAL, HUFFDEC_SUBTABLE_POINTER, LITLEN_DECODE_RESULTS, LITLEN_TABLEBITS,
    OFFSET_DECODE_RESULTS, OFFSET_TABLEBITS, PRECODE_DECODE_RESULTS, PRECODE_TABLEBITS,
    ZLIB_CINFO_32K_WINDOW, ZLIB_CM_DEFLATE, ZLIB_FOOTER_SIZE, bitmask, build_decode_table,
    extract_varbits, extract_varbits8, refill_bits, table_lookup,
};
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
/// It is a count-only copy of the decoder's generic inflate loop on the decoder's own
/// table builders, so it accepts, rejects and consumes what
/// [`Decompressor::deflate_decompress`] does (differential tests and the `fuzz_scan`
/// target hold the two together; the decode loop itself is untouched). With `stop_at`, it returns as soon
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
    match scan_core(&mut d, input, stop_at, &mut fail_at, &stop) {
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

/// The count-only inflate loop: `Decompressor::deflate_decompress_core_impl`'s block
/// parsing and generic loop, with no stores, no fastloop and no double-literal entries
/// (so each symbol's bits are known). Returns `(consumed, out, stopped)`; an error records
/// the input loaded and the output produced in `fail_at`.
fn scan_core(
    d: &mut Decompressor,
    input: &[u8],
    stop_at: usize,
    fail_at: &mut (usize, usize),
    stop: &impl enough::Stop,
) -> Result<(usize, usize, bool), DecompressionError> {
    let mut in_pos: usize = 0;
    let mut out_pos: usize = 0;
    let mut bitbuf: u64 = 0;
    let mut bitsleft: u32 = 0;
    let mut overread_count: usize = 0;
    let bad = DecompressionError::BadData;
    let too_long = DecompressionError::OutputLimitExceeded;
    macro_rules! fail {
        () => {{
            *fail_at = (in_pos, out_pos);
            return Err(bad);
        }};
    }
    macro_rules! refill {
        () => {
            if let Err(e) = refill_bits(
                &mut bitbuf,
                &mut bitsleft,
                input,
                &mut in_pos,
                &mut overread_count,
            ) {
                *fail_at = (input.len(), out_pos);
                return Err(e);
            }
        };
    }
    // `stop_at` output bytes exist: report the input through the last whole byte holding
    // bits of the symbol that produced them.
    macro_rules! stopped {
        () => {{
            let held = (bitsleft / 8) as usize;
            if overread_count > held {
                fail!();
            }
            return Ok((in_pos + overread_count - held, out_pos, true));
        }};
    }

    loop {
        stop.check()?;
        refill!();
        let is_final = (bitbuf & 1) != 0;
        let block_type = ((bitbuf >> 1) & 3) as u32;

        if block_type == DEFLATE_BLOCKTYPE_DYNAMIC_HUFFMAN {
            let num_litlen_syms = 257 + ((bitbuf >> 3) & bitmask(5)) as usize;
            let num_offset_syms = 1 + ((bitbuf >> 8) & bitmask(5)) as usize;
            let num_explicit_precode_lens = 4 + ((bitbuf >> 13) & bitmask(4)) as usize;
            d.static_codes_loaded = false;
            d.precode_lens[DEFLATE_PRECODE_LENS_PERMUTATION[0] as usize] =
                ((bitbuf >> 17) & 7) as u8;
            bitbuf >>= 20;
            bitsleft -= 20;
            refill!();
            for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION[1..num_explicit_precode_lens] {
                d.precode_lens[perm as usize] = (bitbuf & 7) as u8;
                bitbuf >>= 3;
                bitsleft -= 3;
            }
            for &perm in &DEFLATE_PRECODE_LENS_PERMUTATION
                [num_explicit_precode_lens..DEFLATE_NUM_PRECODE_SYMS]
            {
                d.precode_lens[perm as usize] = 0;
            }
            if !build_decode_table(
                &mut d.precode_decode_table,
                &d.precode_lens,
                DEFLATE_NUM_PRECODE_SYMS,
                &PRECODE_DECODE_RESULTS,
                PRECODE_TABLEBITS,
                DEFLATE_MAX_PRE_CODEWORD_LEN,
                &mut d.sorted_syms,
                None,
            ) {
                fail!();
            }
            let total_syms = num_litlen_syms + num_offset_syms;
            let mut i = 0usize;
            while i < total_syms {
                if bitsleft < DEFLATE_MAX_PRE_CODEWORD_LEN + 7 {
                    refill!();
                }
                let entry = d.precode_decode_table
                    [(bitbuf & bitmask(DEFLATE_MAX_PRE_CODEWORD_LEN)) as usize];
                bitbuf >>= (entry & 0xFF) as u64;
                bitsleft -= entry & 0xFF;
                let presym = (entry >> 16) as usize;
                if presym < 16 {
                    d.lens[i] = presym as u8;
                    i += 1;
                    continue;
                }
                if presym == 16 {
                    if i == 0 {
                        fail!();
                    }
                    let rep_val = d.lens[i - 1];
                    let rep_count = 3 + (bitbuf & 3) as usize;
                    bitbuf >>= 2;
                    bitsleft -= 2;
                    d.lens[i..i + 6].fill(rep_val);
                    i += rep_count;
                } else if presym == 17 {
                    let rep_count = 3 + (bitbuf & 7) as usize;
                    bitbuf >>= 3;
                    bitsleft -= 3;
                    d.lens[i..i + 10].fill(0);
                    i += rep_count;
                } else {
                    let rep_count = 11 + (bitbuf & bitmask(7)) as usize;
                    bitbuf >>= 7;
                    bitsleft -= 7;
                    d.lens[i..i + rep_count].fill(0);
                    i += rep_count;
                }
            }
            if i != total_syms {
                fail!();
            }
            if !build_decode_table(
                &mut d.offset_decode_table,
                &d.lens[num_litlen_syms..],
                num_offset_syms,
                &OFFSET_DECODE_RESULTS,
                OFFSET_TABLEBITS,
                15,
                &mut d.sorted_syms,
                None,
            ) || !build_decode_table(
                &mut d.litlen_decode_table,
                &d.lens,
                num_litlen_syms,
                &LITLEN_DECODE_RESULTS,
                LITLEN_TABLEBITS,
                15,
                &mut d.sorted_syms,
                Some(&mut d.litlen_tablebits),
            ) {
                fail!();
            }
        } else if block_type == DEFLATE_BLOCKTYPE_UNCOMPRESSED {
            bitsleft -= 3;
            let extra_bytes = (bitsleft / 8) as usize;
            if overread_count > extra_bytes {
                fail!();
            }
            in_pos -= extra_bytes - overread_count;
            overread_count = 0;
            bitbuf = 0;
            bitsleft = 0;
            if in_pos + 4 > input.len() {
                fail!();
            }
            let len = u16::from_le_bytes([input[in_pos], input[in_pos + 1]]) as usize;
            let nlen = u16::from_le_bytes([input[in_pos + 2], input[in_pos + 3]]);
            in_pos += 4;
            if len != (!nlen) as usize {
                fail!();
            }
            if len > usize::MAX - out_pos {
                return Err(too_long);
            }
            let avail = input.len() - in_pos;
            if len >= stop_at - out_pos && stop_at - out_pos <= avail {
                return Ok((in_pos + (stop_at - out_pos), stop_at, true));
            }
            if len > avail {
                // the stored bytes present are output a streaming decoder delivers
                out_pos += avail;
                in_pos = input.len();
                fail!();
            }
            in_pos += len;
            out_pos += len;
            if is_final {
                break;
            }
            continue;
        } else if block_type == DEFLATE_BLOCKTYPE_STATIC_HUFFMAN {
            bitbuf >>= 3;
            bitsleft -= 3;
            if !d.static_codes_loaded {
                d.static_codes_loaded = true;
                if !d.load_static_tables() {
                    fail!();
                }
            }
        } else {
            fail!();
        }

        // Literals and matches, counted.
        let litlen_tablemask = bitmask(d.litlen_tablebits);
        const STOP_INTERVAL: usize = 16384;
        let mut next_stop_check = out_pos.saturating_add(STOP_INTERVAL);
        loop {
            if out_pos >= next_stop_check {
                stop.check()?;
                next_stop_check = out_pos.saturating_add(STOP_INTERVAL);
            }
            refill!();
            let mut entry = table_lookup(&d.litlen_decode_table, bitbuf & litlen_tablemask);
            let mut saved_bitbuf = bitbuf;
            bitbuf >>= (entry & 0xFF) as u64;
            bitsleft -= entry & 0xFF;
            if entry & (HUFFDEC_LITERAL | HUFFDEC_SUBTABLE_POINTER) == HUFFDEC_SUBTABLE_POINTER {
                entry = table_lookup(
                    &d.litlen_decode_table,
                    (entry >> 16) as u64 + extract_varbits(bitbuf, (entry >> 8) & 0x3F),
                );
                saved_bitbuf = bitbuf;
                bitbuf >>= (entry & 0xFF) as u64;
                bitsleft -= entry & 0xFF;
            }
            if entry & HUFFDEC_LITERAL != 0 {
                if out_pos == usize::MAX {
                    return Err(too_long);
                }
                out_pos += 1;
                if out_pos >= stop_at {
                    stopped!();
                }
                continue;
            }
            if entry & HUFFDEC_END_OF_BLOCK != 0 {
                break;
            }
            let length = (entry >> 16) as usize
                + (extract_varbits8(saved_bitbuf, entry) >> ((entry >> 8) as u8 as u64)) as usize;
            if length > usize::MAX - out_pos {
                return Err(too_long);
            }
            let mut oentry =
                table_lookup(&d.offset_decode_table, bitbuf & bitmask(OFFSET_TABLEBITS));
            if oentry & HUFFDEC_EXCEPTIONAL != 0 {
                bitbuf >>= OFFSET_TABLEBITS as u64;
                bitsleft -= OFFSET_TABLEBITS;
                oentry = table_lookup(
                    &d.offset_decode_table,
                    (oentry >> 16) as u64 + extract_varbits(bitbuf, (oentry >> 8) & 0x3F),
                );
            }
            let saved_bitbuf_off = bitbuf;
            bitbuf >>= (oentry & 0xFF) as u64;
            bitsleft -= oentry & 0xFF;
            let offset = (oentry >> 16) as usize
                + (extract_varbits8(saved_bitbuf_off, oentry) >> ((oentry >> 8) as u8 as u64))
                    as usize;
            if offset == 0 || offset > out_pos {
                fail!();
            }
            out_pos += length;
            if out_pos >= stop_at {
                stopped!();
            }
        }
        if is_final {
            break;
        }
    }
    // Implicit zero bytes must not have been consumed.
    let held = (bitsleft / 8) as usize;
    if overread_count > held {
        fail!();
    }
    Ok((in_pos + overread_count - held, out_pos, false))
}

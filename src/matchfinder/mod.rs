//! Common matchfinder code for LZ77 match finding.
//!
//! Ported from libdeflate's `matchfinder_common.h`.

use archmage::autoversion;

pub(crate) mod bt;
pub(crate) mod fast_ht;
pub(crate) mod hc;
pub(crate) mod ht;
#[cfg(feature = "unchecked")]
pub(crate) mod raw;
pub(crate) mod turbo;

/// Matchfinder window order (log2 of window size).
pub(crate) const MATCHFINDER_WINDOW_ORDER: u32 = 15;

/// Matchfinder window size in bytes.
pub(crate) const MATCHFINDER_WINDOW_SIZE: u32 = 1 << MATCHFINDER_WINDOW_ORDER;

/// Initial value for matchfinder position entries (= -WINDOW_SIZE as i16).
pub(crate) const MATCHFINDER_INITVAL: i16 = i16::MIN;

/// Multiplicative hash function for LZ77 matchfinding.
///
/// Takes a sequence prefix as a 32-bit value and returns the top `num_bits` bits
/// of the product with a carefully chosen constant.
#[inline(always)]
pub(crate) fn lz_hash(seq: u32, num_bits: u32) -> u32 {
    seq.wrapping_mul(0x1E35A7BD) >> (32 - num_bits)
}

/// Extend a match starting at `start_len` up to `max_len`.
///
/// Compares bytes at `strptr[start_len..]` and `matchptr[start_len..]` using
/// word-at-a-time XOR for speed.
#[inline(always)]
pub(crate) fn lz_extend(strptr: &[u8], matchptr: &[u8], start_len: u32, max_len: u32) -> u32 {
    use crate::fast_bytes::{get_byte, load_u64_le};

    let mut len = start_len;
    let max = max_len as usize;

    // Word-at-a-time comparison
    while (len as usize) + 8 <= max {
        let off = len as usize;
        let sw = load_u64_le(strptr, off);
        let mw = load_u64_le(matchptr, off);
        let xor = sw ^ mw;
        if xor != 0 {
            len += xor.trailing_zeros() >> 3;
            return len.min(max_len);
        }
        len += 8;
    }

    // Byte-at-a-time for remainder
    while (len as usize) < max && get_byte(strptr, len as usize) == get_byte(matchptr, len as usize)
    {
        len += 1;
    }
    len
}

/// Initialize a matchfinder table to all MATCHFINDER_INITVAL.
#[autoversion]
pub(crate) fn matchfinder_init(data: &mut [i16]) {
    data.fill(MATCHFINDER_INITVAL);
}

/// Slide the matchfinder window by MATCHFINDER_WINDOW_SIZE.
///
/// Subtracts WINDOW_SIZE from each entry using saturating arithmetic,
/// clamping to -WINDOW_SIZE (keeping out-of-bounds entries permanently invalid).
///
/// Written as `saturating_add(i16::MIN)` so LLVM auto-vectorizes to `vpaddsw`
/// (x86 AVX2), `sqadd` (NEON), or `i16x8_add_sat_s` (WASM simd128).
#[autoversion]
pub(crate) fn matchfinder_rebase(data: &mut [i16]) {
    for entry in data.iter_mut() {
        *entry = entry.saturating_add(i16::MIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lz_hash_deterministic() {
        let h1 = lz_hash(0x12345678, 15);
        let h2 = lz_hash(0x12345678, 15);
        assert_eq!(h1, h2);
        assert!(h1 < (1 << 15));
    }

    #[test]
    fn test_lz_hash_distribution() {
        // Different inputs should generally produce different hashes
        let h1 = lz_hash(0x00000000, 15);
        let h2 = lz_hash(0x00000001, 15);
        let h3 = lz_hash(0xFFFFFFFF, 15);
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
    }

    #[test]
    fn test_lz_extend_identical() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(lz_extend(&data, &data, 0, 10), 10);
    }

    #[test]
    fn test_lz_extend_partial() {
        let a = [1, 2, 3, 4, 5, 6, 7, 8];
        let b = [1, 2, 3, 4, 0, 0, 0, 0];
        assert_eq!(lz_extend(&a, &b, 0, 8), 4);
    }

    #[test]
    fn test_lz_extend_with_start() {
        let a = [0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        let b = [9, 9, 1, 2, 3, 4, 0, 0, 0, 0];
        assert_eq!(lz_extend(&a, &b, 2, 10), 6);
    }

    #[test]
    fn test_matchfinder_init() {
        let mut data = [0i16; 16];
        matchfinder_init(&mut data);
        for &v in &data {
            assert_eq!(v, MATCHFINDER_INITVAL);
        }
    }

    /// Regression: compressing data that crosses the 32K window boundary
    /// with patterns that cause skip_bytes to bail early (not enough trailing
    /// bytes) left in_base_offset stale. The next longest_match call computed
    /// cur_pos > WINDOW_SIZE, bypassing the `== WINDOW_SIZE` slide check.
    ///
    /// Triggers index-out-of-bounds in hc.rs next_tab[cur_pos].
    #[test]
    fn hc_compress_across_window_boundary_no_panic() {
        use crate::compress::{CompressionLevel, Compressor};

        // Build data slightly larger than MATCHFINDER_WINDOW_SIZE (32768).
        // Use repeating pattern to generate matches, with a unique suffix
        // to force the compressor past the window boundary.
        let window = MATCHFINDER_WINDOW_SIZE as usize;
        let mut data = Vec::with_capacity(window + 300);
        // Repeating 256-byte pattern fills most of the window
        let pattern: Vec<u8> = (0..=255u8).collect();
        while data.len() < window - 10 {
            data.extend_from_slice(&pattern);
        }
        // Add unique bytes near the boundary to create skip_bytes edge cases
        for i in 0..310u16 {
            data.push((i & 0xFF) as u8);
        }

        // Try all HC compression levels (1-9 use HC matchfinder)
        for level in 1..=9u32 {
            let mut compressor = Compressor::new(CompressionLevel::new(level));
            let bound = Compressor::zlib_compress_bound(data.len());
            let mut output = vec![0u8; bound];

            let result = compressor.zlib_compress(&data, &mut output, crate::Unstoppable);
            assert!(
                result.is_ok(),
                "compression at level {level} should not panic or error"
            );

            // Verify roundtrip
            let compressed_len = result.unwrap();
            let mut decompressor = crate::Decompressor::new();
            let mut decompressed = vec![0u8; data.len()];
            let dec_result = decompressor.zlib_decompress(
                &output[..compressed_len],
                &mut decompressed,
                crate::Unstoppable,
            );
            assert!(
                dec_result.is_ok(),
                "decompression roundtrip at level {level}"
            );
            assert_eq!(&decompressed, &data, "data roundtrip at level {level}");
        }
    }

    /// Targeted regression: all-zeros input where matches of length 258 land
    /// at position 32766 (= 127 * 258). When total input length is in
    /// [33025, 33028], skip_bytes bails because count(257) + 5 > remaining,
    /// leaving in_base_offset stale at 0. The next longest_match call has
    /// cur_pos = 33024 > 32768, bypassing the `== 32768` slide check.
    ///
    /// Panics with index-out-of-bounds on hc.rs next_tab[33024].
    #[test]
    fn hc_skip_bytes_bail_leaves_stale_base_offset() {
        use crate::compress::{CompressionLevel, Compressor};

        // All-zeros: matches of max length (258) at positions 0, 258, ..., 32766.
        // At position 32766 with total_len=33025: skip_bytes bails because
        //   count(257) + 5 = 262 > in_end(33025) - in_next(32767) = 258
        // Then in_next = 33024, cur_pos = 33024 > 32768 → OOB.
        for total_len in [33025, 33026, 33027, 33028] {
            let data = vec![0u8; total_len];
            for level in 1..=9u32 {
                let mut compressor = Compressor::new(CompressionLevel::new(level));
                let bound = Compressor::zlib_compress_bound(data.len());
                let mut output = vec![0u8; bound];

                let result = compressor.zlib_compress(&data, &mut output, crate::Unstoppable);
                assert!(
                    result.is_ok(),
                    "compression at level {level} with length {total_len} should not panic"
                );

                // Verify roundtrip
                let compressed_len = result.unwrap();
                let mut decompressor = crate::Decompressor::new();
                let mut decompressed = vec![0u8; data.len()];
                let dec_result = decompressor.zlib_decompress(
                    &output[..compressed_len],
                    &mut decompressed,
                    crate::Unstoppable,
                );
                assert!(
                    dec_result.is_ok(),
                    "roundtrip at level {level} with length {total_len}"
                );
                assert_eq!(
                    &decompressed, &data,
                    "data mismatch at level {level} with length {total_len}"
                );
            }
        }
    }

    #[test]
    fn test_matchfinder_rebase() {
        let mut data = [0i16, 100, -100, i16::MAX, i16::MIN, -32768];
        matchfinder_rebase(&mut data);
        // Positive values: v & !(v >> 15) = v, then | 0x8000 = v | -32768
        // For v=0: 0 & !0 = 0 | 0x8000 = -32768
        assert_eq!(data[0], i16::MIN);
        // For v=100: 100 & !0 = 100, 100 | 0x8000 = -32668
        assert_eq!(data[1], 100 | i16::MIN);
        // For v=-100: -100 >> 15 = -1 (all 1s), !(-1) = 0, -100 & 0 = 0, 0 | 0x8000 = -32768
        assert_eq!(data[2], i16::MIN);
        // For v=MAX: stays positive range with sign set
        assert_eq!(data[3], i16::MAX | i16::MIN); // = -1
        // For v=MIN: already negative → clamps to MIN
        assert_eq!(data[4], i16::MIN);
        assert_eq!(data[5], i16::MIN);
    }
}

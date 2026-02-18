//! Common matchfinder code for LZ77 match finding.
//!
//! Ported from libdeflate's `matchfinder_common.h`.

pub(crate) mod bt;
pub(crate) mod hc;
pub(crate) mod ht;

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
#[inline]
pub(crate) fn matchfinder_init(data: &mut [i16]) {
    data.fill(MATCHFINDER_INITVAL);
}

/// Slide the matchfinder window by MATCHFINDER_WINDOW_SIZE.
///
/// Subtracts WINDOW_SIZE from each entry using saturating arithmetic,
/// clamping to -WINDOW_SIZE (keeping out-of-bounds entries permanently invalid).
///
/// Written as `saturating_add(i16::MIN)` so LLVM auto-vectorizes to `vpaddsw`.
#[inline]
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

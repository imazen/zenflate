//! Adler-32 checksum, ported from libdeflate's adler32.c.
//!
//! Uses SIMD acceleration (AVX2) when available via archmage, falling back
//! to a 4-way parallel accumulator scalar pattern.

use archmage::prelude::*;

/// The Adler-32 divisor (largest prime less than 2^16).
const DIVISOR: u32 = 65521;

/// Maximum number of bytes processable without s2 overflowing a u32.
/// Computed assuming worst case: every byte = 0xFF, s1 and s2 start at DIVISOR-1.
const MAX_CHUNK_LEN: usize = 5552;

/// Compute the Adler-32 checksum of `data`, starting from `adler`.
///
/// To compute from scratch, pass `adler = 1` (the Adler-32 initial value).
/// To continue a running checksum, pass the previous return value.
#[allow(unexpected_cfgs)]
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    incant!(adler32_impl(adler, data))
}

// ---------------------------------------------------------------------------
// AVX2 implementation (x86_64-v3: Desktop64 = AVX2+FMA+BMI2)
// ---------------------------------------------------------------------------
#[cfg(target_arch = "x86_64")]
#[arcane]
fn adler32_impl_v3(_token: Desktop64, adler: u32, data: &[u8]) -> u32 {
    use safe_unaligned_simd::x86_64::_mm256_loadu_si256;

    const VL: usize = 32;
    // Max chunk: limit 16-bit byte_sums counters to i16::MAX
    // 2*VL*(i16::MAX/u8::MAX) = 64*128 = 8192. min(8192, 5552) & !63 = 5504
    const MAX_SIMD_CHUNK: usize = {
        let limit = 2 * VL * (i16::MAX as usize / u8::MAX as usize);
        let m = if limit < MAX_CHUNK_LEN {
            limit
        } else {
            MAX_CHUNK_LEN
        };
        m & !(2 * VL - 1)
    };

    // Multiplier tables for pmaddwd. Ordered for 128-bit lane interleaving
    // from vpunpcklbw/vpunpckhbw. Each table has 16 i16 values (one __m256i).
    //
    // When we unpack bytes from a 256-bit vector:
    //   unpacklo gives bytes [0..7] from lane0, [16..23] from lane1
    //   unpackhi gives bytes [8..15] from lane0, [24..31] from lane1
    // data_a covers bytes 0..31, data_b covers bytes 32..63
    // Weights are (2*VL - position) = (64 - position)
    #[repr(align(32))]
    struct Aligned([i16; 16]);

    static MULTS_A: Aligned = Aligned([
        64, 63, 62, 61, 60, 59, 58, 57, 48, 47, 46, 45, 44, 43, 42, 41,
    ]);
    static MULTS_B: Aligned = Aligned([
        56, 55, 54, 53, 52, 51, 50, 49, 40, 39, 38, 37, 36, 35, 34, 33,
    ]);
    static MULTS_C: Aligned = Aligned([
        32, 31, 30, 29, 28, 27, 26, 25, 16, 15, 14, 13, 12, 11, 10, 9,
    ]);
    static MULTS_D: Aligned = Aligned([24, 23, 22, 21, 20, 19, 18, 17, 8, 7, 6, 5, 4, 3, 2, 1]);

    let mults_a = _mm256_loadu_si256(&MULTS_A.0);
    let mults_b = _mm256_loadu_si256(&MULTS_B.0);
    let mults_c = _mm256_loadu_si256(&MULTS_C.0);
    let mults_d = _mm256_loadu_si256(&MULTS_D.0);
    let zeroes = _mm256_setzero_si256();

    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    let mut remaining = data;

    while !remaining.is_empty() {
        let n = remaining.len().min(MAX_SIMD_CHUNK);
        let (chunk, rest) = remaining.split_at(n);
        remaining = rest;

        let mut p = chunk;

        if p.len() >= 2 * VL {
            let mut v_s1 = zeroes;
            let mut v_s1_sums = zeroes;
            let mut v_byte_sums_a = zeroes;
            let mut v_byte_sums_b = zeroes;
            let mut v_byte_sums_c = zeroes;
            let mut v_byte_sums_d = zeroes;

            // Pre-adjust s2 for the vectorized portion
            let vectorized_len = p.len() & !(2 * VL - 1);
            s2 += s1 * vectorized_len as u32;

            while p.len() >= 2 * VL {
                let data_a: &[u8; 32] = p[..32].try_into().unwrap();
                let data_b: &[u8; 32] = p[32..64].try_into().unwrap();
                let va = _mm256_loadu_si256(data_a);
                let vb = _mm256_loadu_si256(data_b);

                // Accumulate s1 sums for s2 weighting
                v_s1_sums = _mm256_add_epi32(v_s1_sums, v_s1);

                // Unpack bytes to 16-bit and accumulate per-position sums
                v_byte_sums_a = _mm256_add_epi16(v_byte_sums_a, _mm256_unpacklo_epi8(va, zeroes));
                v_byte_sums_b = _mm256_add_epi16(v_byte_sums_b, _mm256_unpackhi_epi8(va, zeroes));
                v_byte_sums_c = _mm256_add_epi16(v_byte_sums_c, _mm256_unpacklo_epi8(vb, zeroes));
                v_byte_sums_d = _mm256_add_epi16(v_byte_sums_d, _mm256_unpackhi_epi8(vb, zeroes));

                // Horizontal byte sum via SAD against zero → s1
                let sad_a = _mm256_sad_epu8(va, zeroes);
                let sad_b = _mm256_sad_epu8(vb, zeroes);
                v_s1 = _mm256_add_epi32(v_s1, _mm256_add_epi32(sad_a, sad_b));

                p = &p[2 * VL..];
            }

            // v_s2 = (2*VL)*v_s1_sums + mults . byte_sums
            let v_s2 = {
                let weighted_sums = _mm256_slli_epi32(v_s1_sums, 6); // *64 = 2*VL
                let ma = _mm256_madd_epi16(v_byte_sums_a, mults_a);
                let mb = _mm256_madd_epi16(v_byte_sums_b, mults_b);
                let mc = _mm256_madd_epi16(v_byte_sums_c, mults_c);
                let md = _mm256_madd_epi16(v_byte_sums_d, mults_d);
                let sum_ab = _mm256_add_epi32(ma, mb);
                let sum_cd = _mm256_add_epi32(mc, md);
                _mm256_add_epi32(weighted_sums, _mm256_add_epi32(sum_ab, sum_cd))
            };

            // Reduce 256-bit vectors to scalar s1 and s2
            // Extract high/low 128 and add
            let s1_lo = _mm256_castsi256_si128(v_s1);
            let s1_hi = _mm256_extracti128_si256(v_s1, 1);
            let mut s1_128 = _mm_add_epi32(s1_lo, s1_hi);

            let s2_lo = _mm256_castsi256_si128(v_s2);
            let s2_hi = _mm256_extracti128_si256(v_s2, 1);
            let mut s2_128 = _mm_add_epi32(s2_lo, s2_hi);

            // Horizontal sum: shuffle + add to get all elements into element 0
            // s2: [a, b, c, d] → shuffle(0x31) = [b, a, d, a] → add = [a+b, ?, c+d, ?]
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x31));
            // s1 from SAD: [sum0, 0, sum1, 0] → shuffle(0x02) = [sum1, sum0, sum0, sum0]
            s1_128 = _mm_add_epi32(s1_128, _mm_shuffle_epi32(s1_128, 0x02));
            // s2: [a+b, ?, c+d, ?] → shuffle(0x02) = [c+d, ?, ?, ?] → add = [a+b+c+d, ...]
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x02));

            s1 += _mm_cvtsi128_si32(s1_128) as u32;
            s2 += _mm_cvtsi128_si32(s2_128) as u32;
        }

        // Scalar tail for remaining bytes in this chunk
        adler32_chunk_scalar(&mut s1, &mut s2, p);
    }

    (s2 << 16) | s1
}

// ---------------------------------------------------------------------------
// Scalar fallback
// ---------------------------------------------------------------------------
fn adler32_impl_scalar(_token: ScalarToken, adler: u32, data: &[u8]) -> u32 {
    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    let mut remaining = data;

    while !remaining.is_empty() {
        let chunk_len = remaining.len().min(MAX_CHUNK_LEN & !3);
        let (chunk, rest) = remaining.split_at(chunk_len);
        remaining = rest;
        adler32_chunk_scalar(&mut s1, &mut s2, chunk);
    }

    (s2 << 16) | s1
}

// ---------------------------------------------------------------------------
// Shared scalar chunk processing (used by both SIMD tail and scalar path)
// ---------------------------------------------------------------------------

/// Process a chunk of data, updating s1 and s2, then reduce mod DIVISOR.
///
/// Uses the 4-way parallel accumulator pattern from libdeflate.
fn adler32_chunk_scalar(s1: &mut u32, s2: &mut u32, data: &[u8]) {
    let mut p = data;

    if p.len() >= 4 {
        let mut s1_sum: u32 = 0;
        let mut byte_0_sum: u32 = 0;
        let mut byte_1_sum: u32 = 0;
        let mut byte_2_sum: u32 = 0;
        let mut byte_3_sum: u32 = 0;

        while p.len() >= 4 {
            s1_sum += *s1;
            *s1 += p[0] as u32 + p[1] as u32 + p[2] as u32 + p[3] as u32;
            byte_0_sum += p[0] as u32;
            byte_1_sum += p[1] as u32;
            byte_2_sum += p[2] as u32;
            byte_3_sum += p[3] as u32;
            p = &p[4..];
        }

        *s2 += 4 * (s1_sum + byte_0_sum) + 3 * byte_1_sum + 2 * byte_2_sum + byte_3_sum;
    }

    for &b in p {
        *s1 += b as u32;
        *s2 += *s1;
    }

    *s1 %= DIVISOR;
    *s2 %= DIVISOR;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_value() {
        assert_eq!(adler32(1, &[]), 1);
    }

    #[test]
    fn test_single_byte() {
        assert_eq!(adler32(1, &[0]), (1 << 16) | 1);
        assert_eq!(adler32(1, &[1]), (2 << 16) | 2);
        assert_eq!(adler32(1, &[255]), (256 << 16) | 256);
    }

    #[test]
    fn test_known_values() {
        let data = b"Hello World";
        let result = adler32(1, data);
        assert_eq!(result, libdeflater::adler32(data));
    }

    #[test]
    fn test_incremental() {
        let data = b"Hello World";
        let full = adler32(1, data);
        let partial = adler32(1, &data[..5]);
        let incremental = adler32(partial, &data[5..]);
        assert_eq!(full, incremental);
    }
}

#[cfg(test)]
mod parity {
    use super::*;

    fn check_parity(data: &[u8]) {
        let ours = adler32(1, data);
        let theirs = libdeflater::adler32(data);
        assert_eq!(ours, theirs, "adler32 mismatch for {} bytes", data.len());
    }

    fn check_parity_incremental(data: &[u8], split: usize) {
        let split = split.min(data.len());
        let ours = {
            let a = adler32(1, &data[..split]);
            adler32(a, &data[split..])
        };
        let theirs = libdeflater::adler32(data);
        assert_eq!(
            ours,
            theirs,
            "incremental adler32 mismatch for {} bytes split at {}",
            data.len(),
            split
        );
    }

    #[test]
    fn parity_empty() {
        check_parity(&[]);
    }

    #[test]
    fn parity_single_byte() {
        for b in 0..=255u8 {
            check_parity(&[b]);
        }
    }

    #[test]
    fn parity_all_zeros() {
        for &len in &[1, 100, 5552, 65536] {
            check_parity(&alloc::vec![0u8; len]);
        }
    }

    #[test]
    fn parity_all_ff() {
        for &len in &[1, 100, 5552, 65536] {
            check_parity(&alloc::vec![0xFFu8; len]);
        }
    }

    #[test]
    fn parity_sequential() {
        let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(100_000).collect();
        check_parity(&data);
    }

    #[test]
    fn parity_chunk_boundary() {
        for len in [5550, 5551, 5552, 5553, 5554, 11104, 11105] {
            let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(len).collect();
            check_parity(&data);
        }
    }

    #[test]
    fn parity_incremental() {
        let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(20_000).collect();
        for &split in &[0, 1, 100, 5552, 10000, 20000] {
            check_parity_incremental(&data, split);
        }
    }

    #[test]
    fn parity_large() {
        let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(1_000_000).collect();
        check_parity(&data);
    }
}

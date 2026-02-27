//! Adler-32 checksum, ported from libdeflate's adler32.c.
//!
//! Uses SIMD acceleration when available via archmage:
//! - AVX-512 VNNI 512-bit (x86_64-v4x): `vpdpbusd zmm` for single-instruction dot products
//! - AVX-512 non-VNNI 512-bit (x86_64-v4): `vpsadbw zmm` + `vpunpck zmm` + `vpmaddwd zmm`
//! - AVX2 256-bit (x86_64-v3): `vpsadbw` + `vpunpck` + `vpmaddwd`
//! - NEON 128-bit (aarch64): pairwise add + multiply-accumulate long
//! - WASM SIMD128 (wasm32): extend + pairwise add + dot product
//! - Scalar fallback: 4-way parallel accumulator

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
///
/// ```
/// use zenflate::adler32;
///
/// let checksum = adler32(1, b"Hello");
/// // Continue with more data:
/// let checksum = adler32(checksum, b" World");
/// ```
#[must_use]
#[allow(unexpected_cfgs)]
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    #[cfg(feature = "avx512")]
    {
        incant!(adler32_impl(adler, data), [v4x, v4, v3, neon, wasm128])
    }
    #[cfg(not(feature = "avx512"))]
    {
        incant!(adler32_impl(adler, data), [v3, neon, wasm128])
    }
}

/// Combine two Adler-32 checksums.
///
/// Given `a1 = adler32(1, data1)` and `a2 = adler32(1, data2)`, returns
/// `adler32(1, data1 || data2)` in O(1) time without needing the original data.
/// Used for parallel checksum computation.
#[must_use]
pub fn adler32_combine(adler1: u32, adler2: u32, len2: usize) -> u32 {
    let s1_1 = adler1 & 0xFFFF;
    let s2_1 = adler1 >> 16;
    let s1_2 = adler2 & 0xFFFF;
    let s2_2 = adler2 >> 16;

    // s1 of combined = (s1_1 + s1_2 - 1) mod DIVISOR
    // (subtract 1 because adler32(1, data2) starts s1 at 1, but we want continuation)
    let s1 = (s1_1 + s1_2 + DIVISOR - 1) % DIVISOR;

    // s2 of combined = (s2_1 + s2_2 + s1_1 * len2 - len2) mod DIVISOR
    // The s1_1 * len2 term accounts for s2 accumulating s1_1 for each of the len2 bytes
    // The -len2 removes the initial s1=1 contribution from adler2's computation
    let rem = (len2 % DIVISOR as usize) as u32;
    let s2 = (s2_1 + s2_2 + rem * s1_1 + DIVISOR * 2 - rem) % DIVISOR;

    (s2 << 16) | s1
}

// ---------------------------------------------------------------------------
// AVX-512 VNNI 512-bit (x86_64-v4x: AVX-512 + VNNI)
//
// Uses `vpdpbusd zmm` (dot product of unsigned/signed bytes to i32) for both
// s1 and s2 accumulation. Processes 4*VL=256 bytes per inner loop iteration
// with 4 independent accumulators. 32 ZMM registers = zero spills.
// ---------------------------------------------------------------------------
#[cfg(all(target_arch = "x86_64", feature = "avx512"))]
#[arcane]
#[allow(clippy::incompatible_msrv)]
fn adler32_impl_v4x(_token: X64V4xToken, adler: u32, data: &[u8]) -> u32 {
    use safe_unaligned_simd::x86_64::_mm512_loadu_si512;

    const VL: usize = 64;
    // Round down to multiple of 4*VL = 256
    const MAX_SIMD_CHUNK: usize = MAX_CHUNK_LEN & !(4 * VL - 1);

    // Weight vector: [64, 63, 62, ..., 1] for s2 weighted accumulation
    #[repr(align(64))]
    struct Aligned64([i8; 64]);

    static MULTS: Aligned64 = Aligned64([
        64, 63, 62, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42,
        41, 40, 39, 38, 37, 36, 35, 34, 33, 32, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19,
        18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
    ]);

    let mults = _mm512_loadu_si512(&MULTS.0);
    let ones = _mm512_set1_epi8(1);
    let zeroes = _mm512_setzero_si512();

    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    let mut remaining = data;

    while !remaining.is_empty() {
        let n = remaining.len().min(MAX_SIMD_CHUNK);
        let (chunk, rest) = remaining.split_at(n);
        remaining = rest;

        let mut p = chunk;

        if p.len() >= 4 * VL {
            let mut v_s1_a = zeroes;
            let mut v_s1_b = zeroes;
            let mut v_s1_c = zeroes;
            let mut v_s1_d = zeroes;
            let mut v_s2_a = zeroes;
            let mut v_s2_b = zeroes;
            let mut v_s2_c = zeroes;
            let mut v_s2_d = zeroes;
            let mut v_s1_sums_a = zeroes;
            let mut v_s1_sums_b = zeroes;
            let mut v_s1_sums_c = zeroes;
            let mut v_s1_sums_d = zeroes;

            let vectorized_len = p.len() & !(4 * VL - 1);
            s2 += s1 * vectorized_len as u32;

            while p.len() >= 4 * VL {
                let data_a: &[u8; 64] = p[..64].try_into().unwrap();
                let data_b: &[u8; 64] = p[64..128].try_into().unwrap();
                let data_c: &[u8; 64] = p[128..192].try_into().unwrap();
                let data_d: &[u8; 64] = p[192..256].try_into().unwrap();
                let va = _mm512_loadu_si512(data_a);
                let vb = _mm512_loadu_si512(data_b);
                let vc = _mm512_loadu_si512(data_c);
                let vd = _mm512_loadu_si512(data_d);

                // Track running s1 for cross-iteration s2 weighting
                v_s1_sums_a = _mm512_add_epi32(v_s1_sums_a, v_s1_a);
                v_s1_sums_b = _mm512_add_epi32(v_s1_sums_b, v_s1_b);
                v_s1_sums_c = _mm512_add_epi32(v_s1_sums_c, v_s1_c);
                v_s1_sums_d = _mm512_add_epi32(v_s1_sums_d, v_s1_d);

                // s2: weighted byte sums via vpdpbusd(data, weights)
                v_s2_a = _mm512_dpbusd_epi32(v_s2_a, va, mults);
                v_s2_b = _mm512_dpbusd_epi32(v_s2_b, vb, mults);
                v_s2_c = _mm512_dpbusd_epi32(v_s2_c, vc, mults);
                v_s2_d = _mm512_dpbusd_epi32(v_s2_d, vd, mults);

                // s1: sum of all bytes via vpdpbusd(data, ones)
                v_s1_a = _mm512_dpbusd_epi32(v_s1_a, va, ones);
                v_s1_b = _mm512_dpbusd_epi32(v_s1_b, vb, ones);
                v_s1_c = _mm512_dpbusd_epi32(v_s1_c, vc, ones);
                v_s1_d = _mm512_dpbusd_epi32(v_s1_d, vd, ones);

                p = &p[4 * VL..];
            }

            // Reduce 4 accumulators with position weighting:
            // data_a at offset 0: extra weight = 3*VL per byte
            // data_b at offset VL: extra weight = 2*VL per byte
            // data_c at offset 2*VL: extra weight = VL per byte
            // data_d at offset 3*VL: extra weight = 0
            //
            // Missing s2 = 3*VL*s1_a + 2*VL*s1_b + VL*s1_c
            //            = 2*VL*(s1_a + s1_b) + VL*(s1_a + s1_c)
            let tmp0 = _mm512_add_epi32(v_s1_a, v_s1_b);
            let tmp1 = _mm512_add_epi32(v_s1_a, v_s1_c);

            let total_s1_sums = _mm512_add_epi32(
                _mm512_add_epi32(v_s1_sums_a, v_s1_sums_b),
                _mm512_add_epi32(v_s1_sums_c, v_s1_sums_d),
            );

            let v_s1 = _mm512_add_epi32(_mm512_add_epi32(tmp0, v_s1_c), v_s1_d);

            let v_s2 = {
                let cross_iter = _mm512_slli_epi32(total_s1_sums, 8); // * 256 = 4*VL
                let pos_2vl = _mm512_slli_epi32(tmp0, 7); // * 128 = 2*VL
                let pos_vl = _mm512_slli_epi32(tmp1, 6); // * 64 = VL
                let sum_s2 = _mm512_add_epi32(
                    _mm512_add_epi32(v_s2_a, v_s2_b),
                    _mm512_add_epi32(v_s2_c, v_s2_d),
                );
                _mm512_add_epi32(
                    _mm512_add_epi32(cross_iter, sum_s2),
                    _mm512_add_epi32(pos_2vl, pos_vl),
                )
            };

            // Reduce 512-bit → 256-bit → 128-bit → scalar
            let v_s1_256 = _mm256_add_epi32(
                _mm512_extracti64x4_epi64(v_s1, 0),
                _mm512_extracti64x4_epi64(v_s1, 1),
            );
            let v_s2_256 = _mm256_add_epi32(
                _mm512_extracti64x4_epi64(v_s2, 0),
                _mm512_extracti64x4_epi64(v_s2, 1),
            );

            let mut s1_128 = _mm_add_epi32(
                _mm256_castsi256_si128(v_s1_256),
                _mm256_extracti128_si256(v_s1_256, 1),
            );
            let mut s2_128 = _mm_add_epi32(
                _mm256_castsi256_si128(v_s2_256),
                _mm256_extracti128_si256(v_s2_256, 1),
            );

            // VNNI s1 has values in all 4 lanes
            s1_128 = _mm_add_epi32(s1_128, _mm_shuffle_epi32(s1_128, 0x31));
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x31));
            s1_128 = _mm_add_epi32(s1_128, _mm_shuffle_epi32(s1_128, 0x02));
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x02));

            s1 += _mm_cvtsi128_si32(s1_128) as u32;
            s2 += _mm_cvtsi128_si32(s2_128) as u32;
        }

        adler32_chunk_scalar(&mut s1, &mut s2, p);
    }

    (s2 << 16) | s1
}

// ---------------------------------------------------------------------------
// AVX-512 non-VNNI 512-bit (x86_64-v4: AVX-512F+BW, no VNNI)
//
// Uses `vpsadbw zmm` + `vpunpck{l,h}bw zmm` + `vpmaddwd zmm`.
// Processes 2*VL=128 bytes per inner loop iteration.
// ---------------------------------------------------------------------------
#[cfg(all(target_arch = "x86_64", feature = "avx512"))]
#[arcane]
#[allow(clippy::incompatible_msrv)]
fn adler32_impl_v4(_token: X64V4Token, adler: u32, data: &[u8]) -> u32 {
    use safe_unaligned_simd::x86_64::_mm512_loadu_si512;

    const VL: usize = 64;
    // Limit 16-bit byte_sums counters to i16::MAX:
    // 2*VL*(i16::MAX/u8::MAX) = 128*128 = 16384. min(16384, 5552) & !127 = 5504
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
    // from vpunpcklbw/vpunpckhbw on 512-bit vectors.
    //
    // 512-bit unpacklo takes bytes [0..7] from each 128-bit lane:
    //   Lane 0: bytes 0..7, Lane 1: bytes 16..23, Lane 2: bytes 32..39, Lane 3: bytes 48..55
    // 512-bit unpackhi takes bytes [8..15] from each 128-bit lane:
    //   Lane 0: bytes 8..15, Lane 1: bytes 24..31, Lane 2: bytes 40..47, Lane 3: bytes 56..63
    //
    // data_a covers bytes 0..63, data_b covers bytes 64..127.
    // Weights are (2*VL - position) = (128 - position).
    #[repr(align(64))]
    struct Aligned([i16; 32]);

    // unpacklo(data_a, zero): [0..7, 16..23, 32..39, 48..55]
    static MULTS_A: Aligned = Aligned([
        128, 127, 126, 125, 124, 123, 122, 121, 112, 111, 110, 109, 108, 107, 106, 105, 96, 95, 94,
        93, 92, 91, 90, 89, 80, 79, 78, 77, 76, 75, 74, 73,
    ]);
    // unpackhi(data_a, zero): [8..15, 24..31, 40..47, 56..63]
    static MULTS_B: Aligned = Aligned([
        120, 119, 118, 117, 116, 115, 114, 113, 104, 103, 102, 101, 100, 99, 98, 97, 88, 87, 86,
        85, 84, 83, 82, 81, 72, 71, 70, 69, 68, 67, 66, 65,
    ]);
    // unpacklo(data_b, zero): [64..71, 80..87, 96..103, 112..119]
    static MULTS_C: Aligned = Aligned([
        64, 63, 62, 61, 60, 59, 58, 57, 48, 47, 46, 45, 44, 43, 42, 41, 32, 31, 30, 29, 28, 27, 26,
        25, 16, 15, 14, 13, 12, 11, 10, 9,
    ]);
    // unpackhi(data_b, zero): [72..79, 88..95, 104..111, 120..127]
    static MULTS_D: Aligned = Aligned([
        56, 55, 54, 53, 52, 51, 50, 49, 40, 39, 38, 37, 36, 35, 34, 33, 24, 23, 22, 21, 20, 19, 18,
        17, 8, 7, 6, 5, 4, 3, 2, 1,
    ]);

    let mults_a = _mm512_loadu_si512(&MULTS_A.0);
    let mults_b = _mm512_loadu_si512(&MULTS_B.0);
    let mults_c = _mm512_loadu_si512(&MULTS_C.0);
    let mults_d = _mm512_loadu_si512(&MULTS_D.0);
    let zeroes = _mm512_setzero_si512();

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

            let vectorized_len = p.len() & !(2 * VL - 1);
            s2 += s1 * vectorized_len as u32;

            while p.len() >= 2 * VL {
                let data_a: &[u8; 64] = p[..64].try_into().unwrap();
                let data_b: &[u8; 64] = p[64..128].try_into().unwrap();
                let va = _mm512_loadu_si512(data_a);
                let vb = _mm512_loadu_si512(data_b);

                v_s1_sums = _mm512_add_epi32(v_s1_sums, v_s1);

                // Unpack bytes to 16-bit and accumulate per-position sums
                v_byte_sums_a = _mm512_add_epi16(v_byte_sums_a, _mm512_unpacklo_epi8(va, zeroes));
                v_byte_sums_b = _mm512_add_epi16(v_byte_sums_b, _mm512_unpackhi_epi8(va, zeroes));
                v_byte_sums_c = _mm512_add_epi16(v_byte_sums_c, _mm512_unpacklo_epi8(vb, zeroes));
                v_byte_sums_d = _mm512_add_epi16(v_byte_sums_d, _mm512_unpackhi_epi8(vb, zeroes));

                // Horizontal byte sum via SAD against zero → s1
                let sad_a = _mm512_sad_epu8(va, zeroes);
                let sad_b = _mm512_sad_epu8(vb, zeroes);
                v_s1 = _mm512_add_epi32(v_s1, _mm512_add_epi32(sad_a, sad_b));

                p = &p[2 * VL..];
            }

            // v_s2 = (2*VL)*v_s1_sums + mults . byte_sums
            let v_s2 = {
                let weighted_sums = _mm512_slli_epi32(v_s1_sums, 7); // *128 = 2*VL
                let ma = _mm512_madd_epi16(v_byte_sums_a, mults_a);
                let mb = _mm512_madd_epi16(v_byte_sums_b, mults_b);
                let mc = _mm512_madd_epi16(v_byte_sums_c, mults_c);
                let md = _mm512_madd_epi16(v_byte_sums_d, mults_d);
                let sum_ab = _mm512_add_epi32(ma, mb);
                let sum_cd = _mm512_add_epi32(mc, md);
                _mm512_add_epi32(weighted_sums, _mm512_add_epi32(sum_ab, sum_cd))
            };

            // Reduce 512-bit → 256-bit → 128-bit → scalar
            let v_s1_256 = _mm256_add_epi32(
                _mm512_extracti64x4_epi64(v_s1, 0),
                _mm512_extracti64x4_epi64(v_s1, 1),
            );
            let v_s2_256 = _mm256_add_epi32(
                _mm512_extracti64x4_epi64(v_s2, 0),
                _mm512_extracti64x4_epi64(v_s2, 1),
            );

            let mut s1_128 = _mm_add_epi32(
                _mm256_castsi256_si128(v_s1_256),
                _mm256_extracti128_si256(v_s1_256, 1),
            );
            let mut s2_128 = _mm_add_epi32(
                _mm256_castsi256_si128(v_s2_256),
                _mm256_extracti128_si256(v_s2_256, 1),
            );

            // s2: [a, b, c, d] → shuffle(0x31) = [b, a, d, a] → add = [a+b, ?, c+d, ?]
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x31));
            // s1 from SAD: [sum0, 0, sum1, 0] → shuffle(0x02) = [sum1, sum0, sum0, sum0]
            s1_128 = _mm_add_epi32(s1_128, _mm_shuffle_epi32(s1_128, 0x02));
            // s2: [a+b, ?, c+d, ?] → shuffle(0x02) = [c+d, ?, ?, ?] → add
            s2_128 = _mm_add_epi32(s2_128, _mm_shuffle_epi32(s2_128, 0x02));

            s1 += _mm_cvtsi128_si32(s1_128) as u32;
            s2 += _mm_cvtsi128_si32(s2_128) as u32;
        }

        adler32_chunk_scalar(&mut s1, &mut s2, p);
    }

    (s2 << 16) | s1
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
// NEON implementation (aarch64, basic NEON without dotprod)
//
// Processes 64 bytes per inner loop iteration using pairwise add/accumulate
// and multiply-accumulate long instructions for weighted sums.
// Ported from libdeflate's arm/adler32_impl.h (adler32_arm_neon).
// ---------------------------------------------------------------------------
#[cfg(target_arch = "aarch64")]
#[arcane]
fn adler32_impl_neon(_token: NeonToken, adler: u32, data: &[u8]) -> u32 {
    use safe_unaligned_simd::aarch64::{vld1q_u8, vld1q_u16};

    // Weight tables for s2: position weights [64, 63, ..., 1] split into 8 u16x8 vectors.
    // Within each 64-byte block, byte at position i contributes weight (64 - i) to s2.
    static MULTS_A: [u16; 8] = [64, 63, 62, 61, 60, 59, 58, 57];
    static MULTS_B: [u16; 8] = [56, 55, 54, 53, 52, 51, 50, 49];
    static MULTS_C: [u16; 8] = [48, 47, 46, 45, 44, 43, 42, 41];
    static MULTS_D: [u16; 8] = [40, 39, 38, 37, 36, 35, 34, 33];
    static MULTS_E: [u16; 8] = [32, 31, 30, 29, 28, 27, 26, 25];
    static MULTS_F: [u16; 8] = [24, 23, 22, 21, 20, 19, 18, 17];
    static MULTS_G: [u16; 8] = [16, 15, 14, 13, 12, 11, 10, 9];
    static MULTS_H: [u16; 8] = [8, 7, 6, 5, 4, 3, 2, 1];

    let mults_a = vld1q_u16(&MULTS_A);
    let mults_b = vld1q_u16(&MULTS_B);
    let mults_c = vld1q_u16(&MULTS_C);
    let mults_d = vld1q_u16(&MULTS_D);
    let mults_e = vld1q_u16(&MULTS_E);
    let mults_f = vld1q_u16(&MULTS_F);
    let mults_g = vld1q_u16(&MULTS_G);
    let mults_h = vld1q_u16(&MULTS_H);

    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    let mut remaining = data;

    while !remaining.is_empty() {
        let n = remaining.len().min(MAX_CHUNK_LEN & !63);
        let (chunk, rest) = remaining.split_at(n);
        remaining = rest;
        let mut p = chunk;

        if p.len() >= 64 {
            let mut v_s1 = vdupq_n_u32(0);
            let mut v_s2 = vdupq_n_u32(0);
            // Per-position byte sums across all 64-byte blocks in this chunk.
            // 8 vectors of u16x8 = 64 independent counters.
            let mut v_byte_sums_a = vdupq_n_u16(0);
            let mut v_byte_sums_b = vdupq_n_u16(0);
            let mut v_byte_sums_c = vdupq_n_u16(0);
            let mut v_byte_sums_d = vdupq_n_u16(0);
            let mut v_byte_sums_e = vdupq_n_u16(0);
            let mut v_byte_sums_f = vdupq_n_u16(0);
            let mut v_byte_sums_g = vdupq_n_u16(0);
            let mut v_byte_sums_h = vdupq_n_u16(0);

            // Pre-adjust s2: each of the vectorized_len bytes sees the initial s1.
            let vectorized_len = p.len() & !63;
            s2 += s1 * vectorized_len as u32;

            while p.len() >= 64 {
                let data_a: &[u8; 16] = p[0..16].try_into().unwrap();
                let data_b: &[u8; 16] = p[16..32].try_into().unwrap();
                let data_c: &[u8; 16] = p[32..48].try_into().unwrap();
                let data_d: &[u8; 16] = p[48..64].try_into().unwrap();
                let data_a = vld1q_u8(data_a);
                let data_b = vld1q_u8(data_b);
                let data_c = vld1q_u8(data_c);
                let data_d = vld1q_u8(data_d);

                // Accumulate previous s1 into s2 (the *64 multiplication is delayed)
                v_s2 = vaddq_u32(v_s2, v_s1);

                // Sum bytes to s1 via pairwise add chain:
                // vpaddlq_u8: 16xu8 -> 8xu16 (pairwise add long)
                // vpadalq_u8: accumulate another 16xu8 pairwise into 8xu16
                // vpadalq_u16: accumulate 8xu16 pairwise into 4xu32
                let mut tmp = vpaddlq_u8(data_a);
                v_byte_sums_a = vaddw_u8(v_byte_sums_a, vget_low_u8(data_a));
                v_byte_sums_b = vaddw_u8(v_byte_sums_b, vget_high_u8(data_a));

                tmp = vpadalq_u8(tmp, data_b);
                v_byte_sums_c = vaddw_u8(v_byte_sums_c, vget_low_u8(data_b));
                v_byte_sums_d = vaddw_u8(v_byte_sums_d, vget_high_u8(data_b));

                tmp = vpadalq_u8(tmp, data_c);
                v_byte_sums_e = vaddw_u8(v_byte_sums_e, vget_low_u8(data_c));
                v_byte_sums_f = vaddw_u8(v_byte_sums_f, vget_high_u8(data_c));

                tmp = vpadalq_u8(tmp, data_d);
                v_byte_sums_g = vaddw_u8(v_byte_sums_g, vget_low_u8(data_d));
                v_byte_sums_h = vaddw_u8(v_byte_sums_h, vget_high_u8(data_d));

                v_s1 = vpadalq_u16(v_s1, tmp);

                p = &p[64..];
            }

            // s2 = 64 * s2 + (64*bytesum0 + 63*bytesum1 + ... + 1*bytesum63)
            v_s2 = vqshlq_n_u32::<6>(v_s2);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_a), vget_low_u16(mults_a));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_a, mults_a);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_b), vget_low_u16(mults_b));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_b, mults_b);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_c), vget_low_u16(mults_c));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_c, mults_c);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_d), vget_low_u16(mults_d));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_d, mults_d);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_e), vget_low_u16(mults_e));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_e, mults_e);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_f), vget_low_u16(mults_f));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_f, mults_f);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_g), vget_low_u16(mults_g));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_g, mults_g);
            v_s2 = vmlal_u16(v_s2, vget_low_u16(v_byte_sums_h), vget_low_u16(mults_h));
            v_s2 = vmlal_high_u16(v_s2, v_byte_sums_h, mults_h);

            // Horizontal reduction
            s1 += vaddvq_u32(v_s1);
            s2 += vaddvq_u32(v_s2);
        }

        // Scalar tail for remaining bytes in this chunk
        adler32_chunk_scalar(&mut s1, &mut s2, p);
    }

    (s2 << 16) | s1
}

// ---------------------------------------------------------------------------
// WASM SIMD128 implementation (wasm32)
//
// Uses 128-bit vectors with extend + pairwise add for s1, and
// extend + i32x4_dot_i16x8 (pmaddwd equivalent) for weighted s2.
// Processes 32 bytes per inner loop iteration.
// Ported from libdeflate's SSE2 path (VL=16) adapted for WASM intrinsics.
// ---------------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
#[arcane]
fn adler32_impl_wasm128(_token: Wasm128Token, adler: u32, data: &[u8]) -> u32 {
    use safe_unaligned_simd::wasm32::v128_load;

    const VL: usize = 16;
    // Limit 16-bit byte_sums counters to i16::MAX:
    // 2*VL*(i16::MAX/u8::MAX) = 32*128 = 4096. min(4096, 5552) & !31 = 4096
    const MAX_SIMD_CHUNK: usize = {
        let limit = 2 * VL * (i16::MAX as usize / u8::MAX as usize);
        let m = if limit < MAX_CHUNK_LEN {
            limit
        } else {
            MAX_CHUNK_LEN
        };
        m & !(2 * VL - 1)
    };

    // Weight tables for i32x4_dot_i16x8 (signed i16 multiply-add).
    // extend_low gives bytes [0..7] as u16, extend_high gives bytes [8..15] as u16.
    // data_a covers bytes 0..15, data_b covers bytes 16..31.
    // Weights are (2*VL - position) = (32 - position).
    //
    // WASM extend_low/high doesn't have 128-bit lane issues (only one lane),
    // so the ordering is straightforward.
    static MULTS_A: [i16; 8] = [32, 31, 30, 29, 28, 27, 26, 25];
    static MULTS_B: [i16; 8] = [24, 23, 22, 21, 20, 19, 18, 17];
    static MULTS_C: [i16; 8] = [16, 15, 14, 13, 12, 11, 10, 9];
    static MULTS_D: [i16; 8] = [8, 7, 6, 5, 4, 3, 2, 1];

    let mults_a = v128_load(&MULTS_A);
    let mults_b = v128_load(&MULTS_B);
    let mults_c = v128_load(&MULTS_C);
    let mults_d = v128_load(&MULTS_D);
    let zeroes = i32x4_splat(0);

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
            let mut v_byte_sums_a = i16x8_splat(0);
            let mut v_byte_sums_b = i16x8_splat(0);
            let mut v_byte_sums_c = i16x8_splat(0);
            let mut v_byte_sums_d = i16x8_splat(0);

            let vectorized_len = p.len() & !(2 * VL - 1);
            s2 += s1 * vectorized_len as u32;

            while p.len() >= 2 * VL {
                let data_a: &[u8; 16] = p[..16].try_into().unwrap();
                let data_b: &[u8; 16] = p[16..32].try_into().unwrap();
                let va = v128_load(data_a);
                let vb = v128_load(data_b);

                v_s1_sums = i32x4_add(v_s1_sums, v_s1);

                // Unpack bytes to 16-bit and accumulate per-position sums
                v_byte_sums_a = i16x8_add(v_byte_sums_a, i16x8_extend_low_u8x16(va));
                v_byte_sums_b = i16x8_add(v_byte_sums_b, i16x8_extend_high_u8x16(va));
                v_byte_sums_c = i16x8_add(v_byte_sums_c, i16x8_extend_low_u8x16(vb));
                v_byte_sums_d = i16x8_add(v_byte_sums_d, i16x8_extend_high_u8x16(vb));

                // Horizontal byte sum via pairwise adds → s1
                // u8→u16 pairwise → u16→i32 pairwise
                let sum_a = i32x4_extadd_pairwise_i16x8(i16x8_extadd_pairwise_u8x16(va));
                let sum_b = i32x4_extadd_pairwise_i16x8(i16x8_extadd_pairwise_u8x16(vb));
                v_s1 = i32x4_add(v_s1, i32x4_add(sum_a, sum_b));

                p = &p[2 * VL..];
            }

            // v_s2 = (2*VL)*v_s1_sums + mults . byte_sums
            let v_s2 = {
                let weighted_sums = i32x4_shl(v_s1_sums, 5); // *32 = 2*VL
                let ma = i32x4_dot_i16x8(v_byte_sums_a, mults_a);
                let mb = i32x4_dot_i16x8(v_byte_sums_b, mults_b);
                let mc = i32x4_dot_i16x8(v_byte_sums_c, mults_c);
                let md = i32x4_dot_i16x8(v_byte_sums_d, mults_d);
                let sum_ab = i32x4_add(ma, mb);
                let sum_cd = i32x4_add(mc, md);
                i32x4_add(weighted_sums, i32x4_add(sum_ab, sum_cd))
            };

            // Reduce 128-bit vectors to scalar
            // s1: all 4 lanes have meaningful values from pairwise adds
            s1 += (i32x4_extract_lane::<0>(v_s1)
                + i32x4_extract_lane::<1>(v_s1)
                + i32x4_extract_lane::<2>(v_s1)
                + i32x4_extract_lane::<3>(v_s1)) as u32;
            s2 += (i32x4_extract_lane::<0>(v_s2)
                + i32x4_extract_lane::<1>(v_s2)
                + i32x4_extract_lane::<2>(v_s2)
                + i32x4_extract_lane::<3>(v_s2)) as u32;
        }

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

/// Builder-style Adler-32 hasher — drop-in replacement for `simd_adler32::Adler32`.
///
/// Wraps the SIMD-accelerated [`adler32`] function in a struct that tracks
/// running state and byte count (for [`combine`](Adler32Hasher::combine)).
///
/// ```
/// use zenflate::Adler32Hasher;
///
/// let mut h = Adler32Hasher::new();
/// h.write(b"Hello");
/// h.write(b" World");
/// assert_eq!(h.finish(), zenflate::adler32(1, b"Hello World"));
/// ```
#[derive(Clone, Debug)]
pub struct Adler32Hasher {
    checksum: u32,
    amount: u64,
}

impl Adler32Hasher {
    /// Create a new hasher with the standard initial value (1).
    pub fn new() -> Self {
        Self {
            checksum: 1,
            amount: 0,
        }
    }

    /// Create a hasher seeded with a pre-existing checksum.
    pub fn from_checksum(checksum: u32) -> Self {
        Self {
            checksum,
            amount: 0,
        }
    }

    /// Feed data into the running checksum.
    pub fn write(&mut self, data: &[u8]) {
        self.checksum = adler32(self.checksum, data);
        self.amount += data.len() as u64;
    }

    /// Return the current checksum (non-consuming).
    #[must_use]
    pub fn finish(&self) -> u32 {
        self.checksum
    }

    /// Reset to the initial state.
    pub fn reset(&mut self) {
        self.checksum = 1;
        self.amount = 0;
    }

    /// Combine another hasher's state into this one.
    ///
    /// Equivalent to appending `other`'s input data after `self`'s.
    /// Both hashers must have been started from the initial value (1).
    pub fn combine(&mut self, other: &Self) {
        self.checksum = adler32_combine(self.checksum, other.checksum, other.amount as usize);
        self.amount += other.amount;
    }

    /// Total bytes fed so far.
    pub fn amount(&self) -> u64 {
        self.amount
    }
}

impl Default for Adler32Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl core::hash::Hasher for Adler32Hasher {
    fn finish(&self) -> u64 {
        self.checksum as u64
    }

    fn write(&mut self, bytes: &[u8]) {
        Adler32Hasher::write(self, bytes);
    }
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
    #[cfg_attr(miri, ignore)]
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

    #[test]
    fn hasher_new_write_finish() {
        let mut h = Adler32Hasher::new();
        h.write(b"Hello");
        h.write(b" World");
        assert_eq!(h.finish(), adler32(1, b"Hello World"));
        assert_eq!(h.amount(), 11);
    }

    #[test]
    fn hasher_default() {
        let h = Adler32Hasher::default();
        assert_eq!(h.finish(), 1);
        assert_eq!(h.amount(), 0);
    }

    #[test]
    fn hasher_from_checksum() {
        let partial = adler32(1, b"Hello");
        let mut h = Adler32Hasher::from_checksum(partial);
        h.write(b" World");
        assert_eq!(h.finish(), adler32(1, b"Hello World"));
    }

    #[test]
    fn hasher_reset() {
        let mut h = Adler32Hasher::new();
        h.write(b"data");
        h.reset();
        assert_eq!(h.finish(), 1);
        assert_eq!(h.amount(), 0);
    }

    #[test]
    fn hasher_combine() {
        let mut h1 = Adler32Hasher::new();
        h1.write(b"Hello, ");
        let mut h2 = Adler32Hasher::new();
        h2.write(b"World!");
        h1.combine(&h2);
        assert_eq!(h1.finish(), adler32(1, b"Hello, World!"));
        assert_eq!(h1.amount(), 13);
    }

    #[test]
    fn hasher_core_hash_hasher_trait() {
        use core::hash::Hasher;
        let mut h = Adler32Hasher::new();
        Hasher::write(&mut h, b"Hello World");
        assert_eq!(Hasher::finish(&h), adler32(1, b"Hello World") as u64);
    }

    #[test]
    fn hasher_clone() {
        let mut h = Adler32Hasher::new();
        h.write(b"Hello");
        let h2 = h.clone();
        assert_eq!(h.finish(), h2.finish());
        assert_eq!(h.amount(), h2.amount());
    }

    #[test]
    fn hasher_empty_write() {
        let mut h = Adler32Hasher::new();
        h.write(b"");
        assert_eq!(h.finish(), 1);
        assert_eq!(h.amount(), 0);
    }
}

// All parity tests use libdeflater (C FFI) for comparison.
#[cfg(all(test, not(miri)))]
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

    #[test]
    fn test_adler32_combine_basic() {
        let data1 = b"Hello, ";
        let data2 = b"World!";
        let full = b"Hello, World!";

        let adler_full = super::adler32(1, full);
        let a1 = super::adler32(1, data1);
        let a2 = super::adler32(1, data2);
        let combined = super::adler32_combine(a1, a2, data2.len());
        assert_eq!(combined, adler_full);
    }

    #[test]
    fn test_adler32_combine_large() {
        let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(100_000).collect();
        for split in [1, 100, 1000, 32768, 50000, 99999] {
            let (a, b) = data.split_at(split);
            let adler_full = super::adler32(1, &data);
            let a1 = super::adler32(1, a);
            let a2 = super::adler32(1, b);
            let combined = super::adler32_combine(a1, a2, b.len());
            assert_eq!(combined, adler_full, "failed at split={split}");
        }
    }

    #[test]
    fn test_adler32_combine_empty() {
        let data = b"test data";
        let adler = super::adler32(1, data);
        assert_eq!(super::adler32_combine(adler, 1, 0), adler);
    }

    #[test]
    fn hasher_parity_with_libdeflater() {
        let data: alloc::vec::Vec<u8> = (0..=255).cycle().take(100_000).collect();
        let expected = libdeflater::adler32(&data);

        // Single write
        let mut h = Adler32Hasher::new();
        h.write(&data);
        assert_eq!(h.finish(), expected);

        // Incremental writes
        let mut h = Adler32Hasher::new();
        for chunk in data.chunks(1337) {
            h.write(chunk);
        }
        assert_eq!(h.finish(), expected);

        // Combine
        let (a, b) = data.split_at(50_000);
        let mut h1 = Adler32Hasher::new();
        h1.write(a);
        let mut h2 = Adler32Hasher::new();
        h2.write(b);
        h1.combine(&h2);
        assert_eq!(h1.finish(), expected);
    }

    /// Verify all SIMD dispatch tiers produce identical results to scalar.
    ///
    /// Uses archmage's `for_each_token_permutation` to disable tokens in every
    /// valid combination, then checks that the dispatched result matches
    /// the reference (libdeflater C). Run with `--test-threads=1` for full
    /// correctness (token disabling is process-wide).
    #[test]
    fn adler32_all_simd_tiers() {
        use archmage::testing::{CompileTimePolicy, for_each_token_permutation};

        // Test data at various sizes to exercise scalar tail, SIMD inner loop,
        // and chunk boundary (MAX_CHUNK_LEN = 5552) paths.
        let sizes = [0, 1, 15, 16, 31, 32, 63, 64, 128, 256, 5552, 5553, 100_000];
        let reference: alloc::vec::Vec<u32> = sizes
            .iter()
            .map(|&sz| {
                let data: alloc::vec::Vec<u8> = (0..=255u8).cycle().take(sz).collect();
                libdeflater::adler32(&data)
            })
            .collect();

        let report = for_each_token_permutation(CompileTimePolicy::Warn, |perm| {
            for (i, &sz) in sizes.iter().enumerate() {
                let data: alloc::vec::Vec<u8> = (0..=255u8).cycle().take(sz).collect();
                let result = super::adler32(1, &data);
                assert_eq!(
                    result, reference[i],
                    "adler32 mismatch at size={sz}, tier: {perm}"
                );
            }
        });
        eprintln!("adler32 permutation test: {report}");
    }
}

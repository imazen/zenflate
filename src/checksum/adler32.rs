//! Adler-32 checksum, ported from libdeflate's adler32.c.
//!
//! Uses a 4-way parallel accumulator pattern for increased instruction-level
//! parallelism over the naive `s1 += *p; s2 += s1;` approach.

/// The Adler-32 divisor (largest prime less than 2^16).
const DIVISOR: u32 = 65521;

/// Maximum number of bytes processable without s2 overflowing a u32.
/// Computed assuming worst case: every byte = 0xFF, s1 and s2 start at DIVISOR-1.
const MAX_CHUNK_LEN: usize = 5552;

/// Compute the Adler-32 checksum of `data`, starting from `adler`.
///
/// To compute from scratch, pass `adler = 1` (the Adler-32 initial value).
/// To continue a running checksum, pass the previous return value.
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    let mut s1 = adler & 0xFFFF;
    let mut s2 = adler >> 16;
    let mut remaining = data;

    while !remaining.is_empty() {
        // Process up to MAX_CHUNK_LEN bytes (rounded down to multiple of 4)
        let chunk_len = remaining.len().min(MAX_CHUNK_LEN & !3);
        let (chunk, rest) = remaining.split_at(chunk_len);
        remaining = rest;

        adler32_chunk(&mut s1, &mut s2, chunk);
    }

    (s2 << 16) | s1
}

/// Process a chunk of data, updating s1 and s2, then reduce mod DIVISOR.
///
/// This uses the 4-way parallel accumulator pattern from libdeflate:
/// instead of `s1 += byte; s2 += s1;` per byte, we accumulate partial
/// sums for 4 byte positions and combine them at the end.
#[inline]
fn adler32_chunk(s1: &mut u32, s2: &mut u32, data: &[u8]) {
    let mut p = data;

    // Process 4 bytes at a time
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

    // Process remaining bytes
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
        // For byte b with initial adler=1: s1=1, s2=0
        // After byte: s1 = 1+b, s2 = 0 + (1+b) = 1+b
        // Wait: adler=1 means s1=1, s2=0. After byte b: s1=1+b, s2=1+b.
        // But then reduce mod 65521... for small values no reduction needed.
        assert_eq!(adler32(1, &[0]), (1 << 16) | 1); // s1=1, s2=0+1=1
        assert_eq!(adler32(1, &[1]), (2 << 16) | 2); // s1=2, s2=0+2=2
        assert_eq!(adler32(1, &[255]), (256 << 16) | 256); // s1=256, s2=0+256=256
    }

    #[test]
    fn test_known_values() {
        // Verify against libdeflater
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
            check_parity(&vec![0u8; len]);
        }
    }

    #[test]
    fn parity_all_ff() {
        for &len in &[1, 100, 5552, 65536] {
            check_parity(&vec![0xFFu8; len]);
        }
    }

    #[test]
    fn parity_sequential() {
        let data: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        check_parity(&data);
    }

    #[test]
    fn parity_chunk_boundary() {
        // Test around the MAX_CHUNK_LEN boundary (5552)
        for len in [5550, 5551, 5552, 5553, 5554, 11104, 11105] {
            let data: Vec<u8> = (0..=255).cycle().take(len).collect();
            check_parity(&data);
        }
    }

    #[test]
    fn parity_incremental() {
        let data: Vec<u8> = (0..=255).cycle().take(20_000).collect();
        for &split in &[0, 1, 100, 5552, 10000, 20000] {
            check_parity_incremental(&data, split);
        }
    }

    #[test]
    fn parity_large() {
        let data: Vec<u8> = (0..=255).cycle().take(1_000_000).collect();
        check_parity(&data);
    }
}

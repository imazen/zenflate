//! CRC-32 checksum for gzip, ported from libdeflate's crc32.c.
//!
//! Uses the slice-by-8 method: processes 8 bytes per iteration using
//! 8 lookup tables of 256 entries each.

use super::tables::CRC32_SLICE8_TABLE;

/// Compute the CRC-32 checksum of `data`, continuing from `crc`.
///
/// To compute from scratch, pass `crc = 0`. To continue a running
/// checksum, pass the previous return value.
///
/// This matches `libdeflate_crc32` semantics: the internal CRC state
/// is inverted before and after processing.
///
/// ```
/// use zenflate::crc32;
///
/// let checksum = crc32(0, b"Hello");
/// // Continue with more data:
/// let checksum = crc32(checksum, b" World");
/// ```
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    if data.is_empty() {
        return crc;
    }
    !crc32_slice8(!crc, data)
}

/// Core slice-by-8 CRC-32 implementation.
///
/// Processes data in 8-byte chunks using 8 parallel table lookups,
/// with byte-at-a-time processing for the remainder.
fn crc32_slice8(mut crc: u32, data: &[u8]) -> u32 {
    let table = &CRC32_SLICE8_TABLE;

    // Process leading bytes to make the remainder a multiple of 8
    let lead = data.len() % 8;
    for &b in &data[..lead] {
        crc = (crc >> 8) ^ table[((crc as u8) ^ b) as usize];
    }

    // Main loop: 8 bytes at a time via chunks_exact
    for chunk in data[lead..].chunks_exact(8) {
        let v1 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let v2 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);

        #[allow(clippy::identity_op)]
        {
            crc = table[0x700 + ((crc ^ v1) as u8) as usize]
                ^ table[0x600 + (((crc ^ v1) >> 8) as u8) as usize]
                ^ table[0x500 + (((crc ^ v1) >> 16) as u8) as usize]
                ^ table[0x400 + (((crc ^ v1) >> 24) as u8) as usize]
                ^ table[0x300 + (v2 as u8) as usize]
                ^ table[0x200 + ((v2 >> 8) as u8) as usize]
                ^ table[0x100 + ((v2 >> 16) as u8) as usize]
                ^ table[0x000 + ((v2 >> 24) as u8) as usize];
        }
    }

    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_value() {
        assert_eq!(crc32(0, &[]), 0);
    }

    #[test]
    fn test_known_value() {
        // CRC-32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(0, b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_incremental() {
        let data = b"Hello World";
        let full = crc32(0, data);
        let partial = crc32(0, &data[..5]);
        let incremental = crc32(partial, &data[5..]);
        assert_eq!(full, incremental);
    }
}

#[cfg(test)]
mod parity {
    use super::*;

    fn check_parity(data: &[u8]) {
        let ours = crc32(0, data);
        let theirs = libdeflater::crc32(data);
        assert_eq!(ours, theirs, "crc32 mismatch for {} bytes", data.len());
    }

    fn check_parity_incremental(data: &[u8], split: usize) {
        let split = split.min(data.len());
        let ours = {
            let c = crc32(0, &data[..split]);
            crc32(c, &data[split..])
        };
        let theirs = libdeflater::crc32(data);
        assert_eq!(
            ours,
            theirs,
            "incremental crc32 mismatch for {} bytes split at {}",
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
        for &len in &[1, 100, 8, 16, 64, 65536] {
            check_parity(&vec![0u8; len]);
        }
    }

    #[test]
    fn parity_all_ff() {
        for &len in &[1, 100, 8, 16, 64, 65536] {
            check_parity(&vec![0xFFu8; len]);
        }
    }

    #[test]
    fn parity_sequential() {
        let data: Vec<u8> = (0..=255).cycle().take(100_000).collect();
        check_parity(&data);
    }

    #[test]
    fn parity_alignment_variants() {
        // Test with different amounts of leading bytes before 8-byte chunks
        for offset in 0..16 {
            let data: Vec<u8> = (0..=255).cycle().take(1000 + offset).collect();
            check_parity(&data);
        }
    }

    #[test]
    fn parity_incremental() {
        let data: Vec<u8> = (0..=255).cycle().take(20_000).collect();
        for &split in &[0, 1, 7, 8, 9, 100, 10000, 20000] {
            check_parity_incremental(&data, split);
        }
    }

    #[test]
    fn parity_large() {
        let data: Vec<u8> = (0..=255).cycle().take(1_000_000).collect();
        check_parity(&data);
    }
}

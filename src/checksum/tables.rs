//! Precomputed CRC-32 tables for slice-by-8.
//!
//! The slice-by-8 table is 8 * 256 = 2048 u32 entries.
//! Generated at compile time from the CRC-32 polynomial 0xEDB88320.

/// Slice-by-1 table: CRC of each possible byte value.
const fn generate_slice1_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

/// Slice-by-8 table: 8 subtables of 256 entries each.
/// Subtable 0 = slice-by-1 table.
/// Subtable k = CRC contribution of byte k positions back in an 8-byte group.
const fn generate_slice8_table() -> [u32; 2048] {
    let mut table = [0u32; 2048];

    // Subtable 0 = slice-by-1
    let slice1 = generate_slice1_table();
    let mut i = 0;
    while i < 256 {
        table[i] = slice1[i];
        i += 1;
    }

    // Subtables 1-7: each entry is derived from the previous subtable
    let mut k = 1u32;
    while k < 8 {
        i = 0;
        while i < 256 {
            let prev = table[((k - 1) as usize) * 256 + i];
            table[(k as usize) * 256 + i] = (prev >> 8) ^ table[(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }

    table
}

pub(crate) static CRC32_SLICE8_TABLE: [u32; 2048] = generate_slice8_table();

//! RFC 1951 back-references copy one byte at a time, including overlaps.
use super::fastloop_match_copy;
use alloc::vec::Vec;

#[test]
fn chunked_copy_matches_bytewise_back_references() {
    for offset in (1..=64).chain([255, 256, 32767, 32768]) {
        let mut state = 0x1234_5678u32;
        let initial: Vec<u8> = (0..offset + 258 + 31)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        for length in 3..=258 {
            let mut expected = initial.clone();
            let mut actual = initial.clone();
            for i in 0..length {
                expected[offset + i] = expected[i];
            }
            fastloop_match_copy(&mut actual, offset, 0, length, offset);
            assert_eq!(
                &actual[..offset + length],
                &expected[..offset + length],
                "offset {offset}, length {length}"
            );
        }
    }
}

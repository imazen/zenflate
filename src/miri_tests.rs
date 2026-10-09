//! Bounded checks for the unsafe byte helpers and their compression callers.
//! Run natively too; broad conformance, large-input and C-oracle tests stay in CI.

use crate::{CompressionLevel, Compressor, Decompressor, Unstoppable};

#[test]
fn byte_access_boundaries() {
    let data: Vec<u8> = (0..40).collect();
    for start in 0..8 {
        let input = &data[start..];
        for off in 0..=input.len() - 8 {
            assert_eq!(
                crate::fast_bytes::load_u64_le(input, off),
                u64::from_le_bytes(input[off..off + 8].try_into().unwrap())
            );
            let mut output = vec![0xa5; input.len()];
            crate::fast_bytes::store_u64_le(&mut output, off, 0x0123456789abcdef);
            assert_eq!(&output[off..off + 8], &0x0123456789abcdefu64.to_le_bytes());
            assert!(output[..off].iter().all(|&b| b == 0xa5));
            assert!(output[off + 8..].iter().all(|&b| b == 0xa5));
        }
        for off in 0..=input.len() - 4 {
            let expected = u32::from_le_bytes(input[off..off + 4].try_into().unwrap());
            assert_eq!(crate::fast_bytes::load_u32_le(input, off), expected);
            #[cfg(feature = "unchecked")]
            // SAFETY: each tested access ends within input, including its final byte.
            unsafe {
                assert_eq!(
                    crate::fast_bytes::load_u32_le_ptr(input.as_ptr(), off),
                    expected
                );
            }
        }
        for (off, &expected) in input.iter().enumerate() {
            assert_eq!(crate::fast_bytes::get_byte(input, off), expected);
        }
    }
}

#[cfg(feature = "unchecked")]
#[test]
fn raw_match_extension_boundaries() {
    for len in 0..=33 {
        for mismatch in 0..=len {
            // Offset one exercises unaligned word reads; the allocation ends
            // exactly at max_len, so Miri catches a word read beyond the tail.
            let left = vec![7; len + 1];
            let mut right = left.clone();
            if mismatch < len {
                right[1 + mismatch] = 8;
            }
            // SAFETY: both pointers have len readable bytes after the offset.
            let actual = unsafe {
                crate::matchfinder::raw::lz_extend_raw(
                    left.as_ptr().add(1),
                    right.as_ptr().add(1),
                    0,
                    len as u32,
                )
            };
            assert_eq!(actual as usize, mismatch);
        }
    }
}

#[test]
fn compression_callers_and_reuse() {
    // Representative unchecked callers, including raw-pointer near-optimal paths.
    for level in [
        CompressionLevel::new(1),
        CompressionLevel::new(6),
        CompressionLevel::new(10),
        CompressionLevel::new(15),
        CompressionLevel::new(22),
        CompressionLevel::new(23),
        CompressionLevel::png(1),
        CompressionLevel::png(4),
        CompressionLevel::png(19),
        CompressionLevel::libdeflate(1),
    ] {
        eprintln!("checking {level:?}");
        let mut c = Compressor::new(level);
        // Primitive tests cover word boundaries; here exercise passthrough,
        // actual matchfinding and recovery without repeating full table resets.
        for len in [7, 513] {
            let input: Vec<u8> = (0..len).map(|i| ((i * 13 + i / 31) % 17) as u8).collect();
            assert!(c.deflate_compress(&input, &mut [], Unstoppable).is_err());
            let mut packed = vec![0; Compressor::deflate_compress_bound(len)];
            let n = c
                .deflate_compress(&input, &mut packed, Unstoppable)
                .unwrap();
            let mut output = vec![0; len];
            let decoded = Decompressor::new()
                .deflate_decompress(&packed[..n], &mut output, Unstoppable)
                .unwrap();
            assert_eq!(decoded.output_written, len);
            assert_eq!(output, input);
        }
    }
}

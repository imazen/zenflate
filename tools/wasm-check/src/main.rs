//! Executed in both a native scalar build and optimized WASM; no production oracle dependency.
#![forbid(unsafe_code)]

use zenflate::{CompressionLevel, Compressor, Decompressor, StreamDecompressor, Unstoppable};

fn check(data: &[u8], efforts: &[u32], digest: &mut u32, bytes: &mut Vec<u8>) {
    assert_eq!(zenflate::adler32(1, data), simd_adler32::adler32(&data));
    assert_eq!(zenflate::crc32(0, data), crc32fast::hash(data));
    for split in [0, data.len() / 2, data.len()] {
        let a = zenflate::adler32(1, &data[..split]);
        assert_eq!(
            zenflate::adler32(a, &data[split..]),
            simd_adler32::adler32(&data)
        );
        let c = zenflate::crc32(0, &data[..split]);
        assert_eq!(zenflate::crc32(c, &data[split..]), crc32fast::hash(data));
    }
    for &effort in efforts {
        for level in [CompressionLevel::new(effort), CompressionLevel::png(effort)] {
            let mut c = Compressor::new(level);
            let mut encoded = vec![0; Compressor::zlib_compress_bound(data.len())];
            let n = c.zlib_compress(data, &mut encoded, Unstoppable).unwrap();
            encoded.truncate(n);
            *digest = zenflate::crc32(*digest, &encoded);
            bytes.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&encoded);
            assert_eq!(
                miniz_oxide::inflate::decompress_to_vec_zlib(&encoded).unwrap(),
                data
            );
            let mut out = vec![0; data.len()];
            assert_eq!(
                Decompressor::new()
                    .zlib_decompress(&encoded, &mut out, Unstoppable)
                    .unwrap()
                    .output_written,
                data.len()
            );
            assert_eq!(out, data);
            let mut stream = StreamDecompressor::zlib(encoded.as_slice(), 32768);
            let mut streamed = Vec::new();
            while !stream.is_done() {
                stream.fill().unwrap();
                let chunk = stream.peek();
                streamed.extend_from_slice(chunk);
                let n = chunk.len();
                stream.advance(n);
            }
            assert_eq!(streamed, data);
            for cut in [0, 1, 2, encoded.len() / 2, encoded.len() - 1] {
                assert!(
                    Decompressor::new()
                        .zlib_decompress(&encoded[..cut], &mut out, Unstoppable)
                        .is_err()
                );
            }
            let mut corrupt = encoded.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            assert!(
                Decompressor::new()
                    .zlib_decompress(&corrupt, &mut out, Unstoppable)
                    .is_err()
            );
            if !data.is_empty() {
                assert!(
                    Decompressor::new()
                        .zlib_decompress(&encoded, &mut out[..data.len() - 1], Unstoppable)
                        .is_err()
                );
            }
        }
    }
    // Independent encoder produces known reference streams for our reader.
    for level in [0, 1, 6, 9] {
        let encoded = miniz_oxide::deflate::compress_to_vec_zlib(data, level);
        let mut out = vec![0; data.len()];
        assert_eq!(
            Decompressor::new()
                .zlib_decompress(&encoded, &mut out, Unstoppable)
                .unwrap()
                .output_written,
            data.len()
        );
        assert_eq!(out, data);
    }
    for level in 0..=12 {
        let mut c = Compressor::new(CompressionLevel::libdeflate(level));
        let mut encoded = vec![0; Compressor::deflate_compress_bound(data.len())];
        let n = c.deflate_compress(data, &mut encoded, Unstoppable).unwrap();
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec(&encoded[..n]).unwrap(),
            data
        );
        *digest = zenflate::crc32(*digest, &encoded[..n]);
        bytes.extend_from_slice(&(n as u64).to_le_bytes());
        bytes.extend_from_slice(&encoded[..n]);
        let mut gzip = vec![0; Compressor::gzip_compress_bound(data.len())];
        let n = c.gzip_compress(data, &mut gzip, Unstoppable).unwrap();
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec(&gzip[10..n - 8]).unwrap(),
            data
        );
        let mut out = vec![0; data.len()];
        assert_eq!(
            Decompressor::new()
                .gzip_decompress(&gzip[..n], &mut out, Unstoppable)
                .unwrap()
                .output_written,
            data.len()
        );
        assert_eq!(out, data);
        *digest = zenflate::crc32(*digest, &gzip[..n]);
        bytes.extend_from_slice(&(n as u64).to_le_bytes());
        bytes.extend_from_slice(&gzip[..n]);
    }
}

fn main() {
    let mut digest = 0;
    let mut bytes = Vec::new();
    let all: Vec<u32> = (0..=31).collect();
    for n in [0, 1, 15, 16, 17, 31, 32, 33, 257, 258, 259, 1024] {
        for pattern in 0..3 {
            let data: Vec<u8> = (0..n)
                .map(|i| match pattern {
                    0 => 255,
                    1 => ((i * 37 + i / 7) & 255) as u8,
                    _ => {
                        if i % 67 < 60 {
                            (i % 5) as u8
                        } else {
                            (i * 113) as u8
                        }
                    }
                })
                .collect();
            check(&data, &all, &mut digest, &mut bytes);
        }
    }
    for n in [
        4095, 4096, 4097, 5551, 5552, 5553, 32767, 32768, 32769, 65535, 65536, 65537,
    ] {
        let data: Vec<u8> = (0..n)
            .map(|i| if i % 71 < 64 { 255 } else { (i * 37) as u8 })
            .collect();
        check(
            &data,
            &[0, 1, 5, 10, 15, 22, 23, 30],
            &mut digest,
            &mut bytes,
        );
    }
    for malformed in [&[7u8][..], &[0x78, 0x9c, 7][..], &[0x78, 0x00][..]] {
        assert!(
            Decompressor::new()
                .zlib_decompress(malformed, &mut [0; 64], Unstoppable)
                .is_err()
        );
    }
    if let Some(expected) = option_env!("ZENFLATE_EXPECTED_DIGEST") {
        assert_eq!(
            digest,
            expected.parse::<u32>().unwrap(),
            "scalar/SIMD encoded byte parity"
        );
    }
    #[cfg(target_arch = "wasm32")]
    assert_eq!(
        bytes.as_slice(),
        include_bytes!(env!("ZENFLATE_EXPECTED_BYTES")),
        "scalar/SIMD exact encoded bytes"
    );
    #[cfg(not(target_arch = "wasm32"))]
    std::fs::write(std::env::var("ZENFLATE_EXPECTED_BYTES").unwrap(), &bytes).unwrap();
    #[cfg(not(target_arch = "wasm32"))]
    println!("{digest}");
}

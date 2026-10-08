//! A checksum result belongs to the latest decode, including failed calls.
#![cfg(feature = "compress")]

use zenflate::{ChecksumPolicy, CompressionLevel, Compressor, Decompressor, Unstoppable};

#[test]
fn checksum_result_is_cleared_before_each_decode() {
    let data = b"a previous successful stream must not supply the next checksum result";
    let mut c = Compressor::new(CompressionLevel::fast());
    let mut z = vec![0; Compressor::zlib_compress_bound(data.len())];
    let zn = c.zlib_compress(data, &mut z, Unstoppable).unwrap();
    let z = &z[..zn];
    let mut g = vec![0; Compressor::gzip_compress_bound(data.len())];
    let gn = c.gzip_compress(data, &mut g, Unstoppable).unwrap();
    let g = &g[..gn];
    let mut raw = vec![0; Compressor::deflate_compress_bound(data.len())];
    let rn = c.deflate_compress(data, &mut raw, Unstoppable).unwrap();
    for policy in [ChecksumPolicy::Verify, ChecksumPolicy::Report] {
        for next in 0..5 {
            let mut d = Decompressor::new().with_checksum(policy);
            let mut out = vec![0; data.len()];
            d.zlib_decompress(z, &mut out, Unstoppable).unwrap();
            assert_eq!(d.checksum_matched(), Some(true));
            match next {
                0 => {
                    d.deflate_decompress(&raw[..rn], &mut out, Unstoppable)
                        .unwrap();
                }
                1 => {
                    assert!(d.zlib_decompress(&[], &mut out, Unstoppable).is_err());
                }
                2 => {
                    assert!(d.gzip_decompress(&[], &mut out, Unstoppable).is_err());
                }
                3 => {
                    assert!(d.zlib_decompress(z, &mut [], Unstoppable).is_err());
                }
                _ => {
                    assert!(d.gzip_decompress(g, &mut [], Unstoppable).is_err());
                }
            }
            assert_eq!(d.checksum_matched(), None, "policy={policy:?}, next={next}");
        }
    }
}

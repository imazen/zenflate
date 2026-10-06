//! Independent DEFLATE segments: `Compressor::deflate_compress_segment` and
//! `StreamDecompressor::with_segment_end`.
//!
//! The property under test is serial equivalence: decoding the concatenated
//! segments as one stream must produce exactly what decoding each segment on
//! its own produces, and any segment that would decode differently on its own
//! must be rejected rather than accepted.

use zenflate::{
    CompressionLevel, Compressor, Decompressor, StreamDecompressor, Unstoppable, adler32,
    adler32_combine,
};

fn test_data(len: usize) -> Vec<u8> {
    // Mixed content: repeats (matches), a slow ramp, and noise.
    let mut v = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for i in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let b = match (i / 4096) % 3 {
            0 => (i % 251) as u8,
            1 => (i / 64) as u8,
            _ => x as u8,
        };
        v.push(b);
    }
    v
}

/// `parts` equal pieces (the last may be shorter), compressed as segments by
/// one reused compressor.
fn compress_segments(data: &[u8], parts: usize, level: CompressionLevel) -> Vec<Vec<u8>> {
    let per = data.len().div_ceil(parts).max(1);
    let chunks: Vec<&[u8]> = data.chunks(per).collect();
    let n = chunks.len();
    let mut comp = Compressor::new(level);
    chunks
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let mut out = vec![0u8; Compressor::deflate_compress_segment_bound(c.len())];
            let len = comp
                .deflate_compress_segment(c, k + 1 == n, &mut out, Unstoppable)
                .unwrap();
            out.truncate(len);
            out
        })
        .collect()
}

/// Decode one segment with segment mode on; returns (output, ended_at_boundary).
fn decode_segment(seg: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let mut d = StreamDecompressor::deflate(seg, 64 * 1024).with_segment_end(true);
    let mut out = Vec::new();
    while !d.is_done() {
        let got = d.fill().map_err(|e| format!("{e:?}"))?;
        let n = got.len();
        out.extend_from_slice(got);
        d.advance(n);
    }
    Ok((out, d.ended_at_segment_boundary()))
}

fn levels() -> Vec<(String, CompressionLevel)> {
    let mut v: Vec<(String, CompressionLevel)> = [0u32, 1, 2, 5, 9, 10, 15, 22, 30]
        .iter()
        .map(|&e| (format!("effort {e}"), CompressionLevel::new(e)))
        .collect();
    v.push(("effort 31 (FullOptimal)".into(), CompressionLevel::new(31)));
    v
}

#[test]
fn segments_concatenate_to_one_valid_stream_and_decode_independently() {
    let data = test_data(300_000);
    for (name, level) in levels() {
        for parts in [1usize, 2, 3, 8] {
            let segs = compress_segments(&data, parts, level);
            let n = segs.len();

            // Serial: one stream.
            let joined: Vec<u8> = segs.concat();
            let mut out = vec![0u8; data.len()];
            let r = Decompressor::new()
                .deflate_decompress(&joined, &mut out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name} parts={parts}: serial decode failed: {e:?}"));
            assert_eq!(r.output_written, data.len(), "{name} parts={parts}");
            assert_eq!(r.input_consumed, joined.len(), "{name} parts={parts}");
            assert_eq!(out, data, "{name} parts={parts}");

            // Parallel interpretation: each segment on its own.
            let mut rebuilt = Vec::new();
            for (k, seg) in segs.iter().enumerate() {
                let (o, ended) = decode_segment(seg)
                    .unwrap_or_else(|e| panic!("{name} parts={parts} seg {k}: {e}"));
                assert_eq!(
                    ended,
                    k + 1 < n,
                    "{name} parts={parts} seg {k} boundary flag"
                );
                rebuilt.extend_from_slice(&o);
            }
            assert_eq!(rebuilt, data, "{name} parts={parts}: parallel != serial");
        }
    }
}

#[test]
fn zlib_framing_with_combined_adler() {
    let data = test_data(200_000);
    let segs = compress_segments(&data, 4, CompressionLevel::new(10));
    let per = data.len().div_ceil(4);
    let mut adler = 1u32;
    for c in data.chunks(per) {
        adler = adler32_combine(adler, adler32(1, c), c.len());
    }
    assert_eq!(adler, adler32(1, &data));
    let mut z = vec![0x78, 0x9c];
    for s in &segs {
        z.extend_from_slice(s);
    }
    z.extend_from_slice(&adler.to_be_bytes());
    let mut out = vec![0u8; data.len()];
    Decompressor::new()
        .zlib_decompress(&z, &mut out, Unstoppable)
        .unwrap();
    assert_eq!(out, data);

    // The first segment, zlib-wrapped and decoded in segment mode, ends at
    // the boundary without reading a footer.
    let first_len = 2 + segs[0].len();
    let mut d = StreamDecompressor::zlib(&z[..first_len], 64 * 1024).with_segment_end(true);
    let mut got = Vec::new();
    while !d.is_done() {
        let o = d.fill().unwrap();
        let n = o.len();
        got.extend_from_slice(o);
        d.advance(n);
    }
    assert!(d.ended_at_segment_boundary());
    assert_eq!(d.checksum_matched(), None);
    assert_eq!(got, &data[..per]);
}

#[test]
fn segment_mode_off_still_reports_truncation() {
    let data = test_data(50_000);
    let segs = compress_segments(&data, 2, CompressionLevel::new(6));
    let mut d = StreamDecompressor::deflate(&segs[0][..], 64 * 1024);
    let mut failed = false;
    while !d.is_done() {
        match d.fill() {
            Ok(o) => {
                let n = o.len();
                if n == 0 {
                    break;
                }
                d.advance(n);
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(
        failed || !d.is_done(),
        "non-final segment must not look complete"
    );
    assert!(!d.ended_at_segment_boundary());
}

/// A segment that ends inside a stored block's header/payload split — the
/// "ambiguous PNG" construction — must not end cleanly: a serial decoder
/// would read the next segment's bytes as literal stored data.
#[test]
fn segment_ending_inside_stored_block_is_rejected() {
    // Non-final stored block header claiming 10 bytes, with no payload.
    let seg = [0x00u8, 10, 0, !10u8, 0xff];
    assert!(decode_segment(&seg).is_err());

    // A valid segment followed by a dangling stored header.
    let data = test_data(10_000);
    let mut segs = compress_segments(&data, 2, CompressionLevel::new(6));
    segs[0].extend_from_slice(&[0x00, 10, 0, !10u8, 0xff]);
    assert!(decode_segment(&segs[0]).is_err());
}

/// A non-final segment whose last block is BFINAL ends normally, not at a
/// segment boundary — callers must treat that as "not a valid segment".
#[test]
fn bfinal_inside_segment_is_not_a_segment_boundary() {
    let data = test_data(20_000);
    let mut c = Compressor::new(CompressionLevel::new(6));
    let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
    let n = c.deflate_compress(&data, &mut out, Unstoppable).unwrap();
    let (o, ended) = decode_segment(&out[..n]).unwrap();
    assert_eq!(o, data);
    assert!(!ended);
}

/// Dropping the flush marker's last bytes leaves the stream mid-block or with
/// leftover bits; that must never be reported as a clean segment end unless
/// the remaining prefix genuinely ends byte-aligned at a block boundary (in
/// which case the output must equal the original segment's data).
#[test]
fn truncated_flush_marker_never_misreports() {
    let data = test_data(30_000);
    for level in [1u32, 6, 12] {
        let segs = compress_segments(&data, 2, CompressionLevel::new(level));
        let per = data.len().div_ceil(2);
        for cut in 1..=6 {
            let s = &segs[0][..segs[0].len() - cut];
            if let Ok((o, true)) = decode_segment(s) {
                assert_eq!(
                    o,
                    &data[..per],
                    "level {level} cut {cut}: clean end with wrong data"
                );
            }
        }
    }
}

/// A later segment that back-references data from an earlier segment is only
/// decodable serially; decoding it alone must fail rather than read zeros.
#[test]
fn cross_segment_back_reference_is_rejected() {
    // Segment 0: a non-final stored block holding 100 bytes (byte-aligned).
    let a: Vec<u8> = (0..100u8).collect();
    let mut seg0 = vec![0x00u8, 100, 0, !100u8, !0u8];
    seg0.extend_from_slice(&a);
    // Segment 1: static Huffman, BFINAL=1, one match (length 3, distance 1)
    // then EOB. Bits are LSB-first; Huffman codes are written MSB-first.
    let mut bits: Vec<u8> = vec![1, 1, 0];
    let push_code = |bits: &mut Vec<u8>, code: u32, len: u32| {
        for i in (0..len).rev() {
            bits.push(((code >> i) & 1) as u8);
        }
    };
    push_code(&mut bits, 0b0000001, 7); // length 3
    push_code(&mut bits, 0, 5); // distance 1
    push_code(&mut bits, 0, 7); // EOB
    let mut seg1 = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        seg1[i / 8] |= b << (i % 8);
    }
    // Serially (after seg0's 100 bytes) this is valid.
    let joined: Vec<u8> = [seg0.clone(), seg1.clone()].concat();
    let mut out = vec![0u8; 103];
    let r = Decompressor::new()
        .deflate_decompress(&joined, &mut out, Unstoppable)
        .unwrap();
    assert_eq!(r.output_written, 103);
    // seg0 alone ends at a segment boundary.
    let (o0, ended) = decode_segment(&seg0).unwrap();
    assert!(ended);
    assert_eq!(o0, a);
    // seg1 alone must fail.
    assert!(decode_segment(&seg1).is_err());
}

/// The tail of a zlib stream decoded on its own: footer recorded, not
/// verified; per-segment checksums combine to the footer value.
#[test]
fn zlib_continuation_reports_partial_and_footer_checksums() {
    let data = test_data(120_000);
    let segs = compress_segments(&data, 3, CompressionLevel::new(8));
    let per = data.len().div_ceil(3);
    let adler = adler32(1, &data);
    let mut z = vec![0x78, 0x9c];
    let mut bounds = vec![];
    for s in &segs {
        let start = z.len();
        z.extend_from_slice(s);
        bounds.push((start, z.len()));
    }
    z.extend_from_slice(&adler.to_be_bytes());

    let decode = |mut d: StreamDecompressor<&[u8]>| {
        let mut out = Vec::new();
        while !d.is_done() {
            let o = d.fill().unwrap();
            let n = o.len();
            out.extend_from_slice(o);
            d.advance(n);
        }
        (
            out,
            d.running_checksum(),
            d.footer_checksum(),
            d.ended_at_segment_boundary(),
            d.checksum_matched(),
        )
    };
    let (o0, a0, f0, e0, m0) =
        decode(StreamDecompressor::zlib(&z[..bounds[0].1], 64 * 1024).with_segment_end(true));
    let (o1, a1, f1, e1, _) = decode(
        StreamDecompressor::zlib_continuation(&z[bounds[1].0..bounds[1].1], 64 * 1024)
            .with_segment_end(true),
    );
    let (o2, a2, f2, e2, m2) = decode(
        StreamDecompressor::zlib_continuation(&z[bounds[2].0..], 64 * 1024).with_segment_end(true),
    );
    assert_eq!([o0, o1, o2].concat(), data);
    assert!(e0 && e1 && !e2);
    assert_eq!((f0, f1, f2), (None, None, Some(adler)));
    assert_eq!((m0, m2), (None, None));
    let combined = adler32_combine(adler32_combine(a0, a1, per), a2, data.len() - 2 * per);
    assert_eq!(combined, adler);

    // A wrong footer is reported, not raised.
    let mut bad = z[bounds[2].0..].to_vec();
    let n = bad.len();
    bad[n - 1] ^= 1;
    let (_, _, fb, _, mb) = decode(StreamDecompressor::zlib_continuation(&bad[..], 64 * 1024));
    assert_eq!(fb, Some(adler ^ 1));
    assert_eq!(mb, None);
}

/// A compressor reused across segments (as a caller compressing strips on a
/// pool would) gives the same bytes as a fresh compressor per segment.
#[test]
fn segment_output_does_not_depend_on_reuse() {
    let data = test_data(250_000);
    for (name, level) in levels() {
        let reused = compress_segments(&data, 5, level);
        let per = data.len().div_ceil(5);
        let n = reused.len();
        for (k, c) in data.chunks(per).enumerate() {
            let mut out = vec![0u8; Compressor::deflate_compress_segment_bound(c.len())];
            let len = Compressor::new(level)
                .deflate_compress_segment(c, k + 1 == n, &mut out, Unstoppable)
                .unwrap();
            assert_eq!(&out[..len], &reused[k][..], "{name} segment {k}");
        }
    }
}

/// Empty and tiny segments, in the middle and at the end.
#[test]
fn empty_and_tiny_segments() {
    let data = test_data(20_000);
    for lens in [
        vec![0usize],
        vec![0, 20_000],
        vec![5_000, 0, 1, 14_999],
        vec![20_000, 0],
    ] {
        let mut c = Compressor::new(CompressionLevel::new(10));
        let mut z = Vec::new();
        let mut start = 0;
        for (k, &l) in lens.iter().enumerate() {
            let seg = &data[start..start + l];
            let mut out = vec![0u8; Compressor::deflate_compress_segment_bound(l)];
            let n = c
                .deflate_compress_segment(seg, k + 1 == lens.len(), &mut out, Unstoppable)
                .unwrap();
            z.extend_from_slice(&out[..n]);
            start += l;
        }
        let mut back = vec![0u8; start];
        let r = Decompressor::new()
            .deflate_decompress(&z, &mut back, Unstoppable)
            .unwrap_or_else(|e| panic!("{lens:?}: {e:?}"));
        assert_eq!(r.output_written, start);
        assert_eq!(back, data[..start], "{lens:?}");
    }
}

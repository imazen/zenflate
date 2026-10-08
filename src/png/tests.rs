//! `png::StripCompressor`: strips must concatenate into one valid stream
//! and each must decode on its own to exactly its slice of the input.
//!
//! Independence is checked with the ordinary raw decoder: a non-final strip
//! followed by an empty final block (`03 00`) decodes in full only if the
//! strip ends byte-aligned on a block boundary with no final block, and a
//! back-reference before the strip's start is a decode error.

use super::{StripCompressor, StripDecoder};
use crate::{
    CompressionLevel, Compressor, Decompressor, StreamDecompressor, Unstoppable, adler32,
    adler32_combine,
};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

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

/// `parts` equal pieces (the last may be shorter), compressed as strips by
/// one reused compressor.
fn compress_strips(data: &[u8], parts: usize, level: CompressionLevel) -> Vec<Vec<u8>> {
    let per = data.len().div_ceil(parts).max(1);
    let chunks: Vec<&[u8]> = data.chunks(per).collect();
    let n = chunks.len();
    let mut comp = StripCompressor::new(level);
    chunks
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let mut out = vec![0u8; StripCompressor::bound(c.len())];
            let len = comp.compress(c, k + 1 == n, &mut out, Unstoppable).unwrap();
            out.truncate(len);
            out
        })
        .collect()
}

/// Decode one strip on its own with an empty window. A non-final strip gets
/// an empty final fixed-Huffman block appended; every input byte must be
/// consumed, so a final block inside the strip or a misaligned end fails.
fn decode_strip(strip: &[u8], is_last: bool, expected_len: usize) -> Result<Vec<u8>, String> {
    let mut input = strip.to_vec();
    if !is_last {
        input.extend_from_slice(&[0x03, 0x00]);
    }
    let mut out = vec![0u8; expected_len];
    let r = Decompressor::new()
        .deflate_decompress(&input, &mut out, Unstoppable)
        .map_err(|e| format!("{e:?}"))?;
    if r.input_consumed != input.len() {
        return Err(format!("consumed {} of {}", r.input_consumed, input.len()));
    }
    out.truncate(r.output_written);
    Ok(out)
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
fn strips_concatenate_to_one_valid_stream_and_decode_independently() {
    let data = test_data(300_000);
    for (name, level) in levels() {
        for parts in [1usize, 2, 3, 8] {
            let strips = compress_strips(&data, parts, level);
            let per = data.len().div_ceil(parts);
            let n = strips.len();

            // Serial: one stream.
            let joined: Vec<u8> = strips.concat();
            let mut out = vec![0u8; data.len()];
            let r = Decompressor::new()
                .deflate_decompress(&joined, &mut out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name} parts={parts}: serial decode failed: {e:?}"));
            assert_eq!(r.output_written, data.len(), "{name} parts={parts}");
            assert_eq!(r.input_consumed, joined.len(), "{name} parts={parts}");
            assert_eq!(out, data, "{name} parts={parts}");

            // Each strip on its own.
            for (k, (strip, chunk)) in strips.iter().zip(data.chunks(per)).enumerate() {
                let o = decode_strip(strip, k + 1 == n, chunk.len())
                    .unwrap_or_else(|e| panic!("{name} parts={parts} strip {k}: {e}"));
                assert_eq!(o, chunk, "{name} parts={parts} strip {k}");
            }
        }
    }
}

#[test]
fn zlib_framing_with_combined_adler() {
    let data = test_data(200_000);
    let segs = compress_strips(&data, 4, CompressionLevel::new(10));
    let per = data.len().div_ceil(4);
    let mut adler = 1u32;
    for c in data.chunks(per) {
        adler = adler32_combine(adler, adler32(1, c), c.len());
    }
    assert_eq!(adler, adler32(1, &data));
    let mut z = StripCompressor::new(CompressionLevel::new(10))
        .zlib_header()
        .to_vec();
    for s in &segs {
        z.extend_from_slice(s);
    }
    z.extend_from_slice(&adler.to_be_bytes());
    let mut out = vec![0u8; data.len()];
    Decompressor::new()
        .zlib_decompress(&z, &mut out, Unstoppable)
        .unwrap();
    assert_eq!(out, data);
}

/// A strip ending in a dangling stored-block header (the "ambiguous PNG"
/// construction) must fail the independence check.
#[test]
fn decode_strip_rejects_misframed_strips() {
    let data = test_data(10_000);
    let mut strips = compress_strips(&data, 2, CompressionLevel::new(6));
    assert!(decode_strip(&strips[0], false, 5_000).is_ok());
    strips[0].extend_from_slice(&[0x00, 10, 0, !10u8, 0xff]);
    assert!(decode_strip(&strips[0], false, 5_010).is_err());
    // A whole stream (final block inside) is not a non-final strip.
    let mut out = vec![0u8; Compressor::deflate_compress_bound(data.len())];
    let n = Compressor::new(CompressionLevel::new(6))
        .deflate_compress(&data, &mut out, Unstoppable)
        .unwrap();
    assert!(decode_strip(&out[..n], false, data.len()).is_err());
}

/// A compressor reused across strips (as a caller compressing strips on a
/// pool would) gives the same bytes as a fresh compressor per strip.
#[test]
fn strip_output_does_not_depend_on_reuse() {
    let data = test_data(250_000);
    for (name, level) in levels() {
        let reused = compress_strips(&data, 5, level);
        let per = data.len().div_ceil(5);
        let n = reused.len();
        for (k, c) in data.chunks(per).enumerate() {
            let mut out = vec![0u8; StripCompressor::bound(c.len())];
            let len = StripCompressor::new(level)
                .compress(c, k + 1 == n, &mut out, Unstoppable)
                .unwrap();
            assert_eq!(&out[..len], &reused[k][..], "{name} strip {k}");
        }
    }
}

/// Empty and tiny strips, in the middle and at the end.
#[test]
fn empty_and_tiny_strips() {
    let data = test_data(20_000);
    for lens in [
        vec![0usize],
        vec![0, 20_000],
        vec![5_000, 0, 1, 14_999],
        vec![20_000, 0],
    ] {
        let mut c = StripCompressor::new(CompressionLevel::new(10));
        let mut z = Vec::new();
        let mut start = 0;
        for (k, &l) in lens.iter().enumerate() {
            let seg = &data[start..start + l];
            let mut out = vec![0u8; StripCompressor::bound(l)];
            let last = k + 1 == lens.len();
            let n = c.compress(seg, last, &mut out, Unstoppable).unwrap();
            let alone =
                decode_strip(&out[..n], last, l).unwrap_or_else(|e| panic!("{lens:?} {k}: {e}"));
            assert_eq!(alone, seg, "{lens:?} strip {k} alone");
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

/// The strip header is the one `zlib_compress` writes at the same level.
#[test]
fn strip_header_matches_zlib_compress() {
    let data = test_data(1000);
    for e in [0u32, 1, 4, 10, 22, 31] {
        for level in [CompressionLevel::new(e), CompressionLevel::png(e)] {
            let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
            Compressor::new(level)
                .zlib_compress(&data, &mut out, Unstoppable)
                .unwrap();
            assert_eq!(
                StripCompressor::new(level).zlib_header(),
                out[..2],
                "{level:?}"
            );
        }
    }
}

/// Decode one raw-DEFLATE segment with the crate-private segment mode that
/// [`StripDecoder`](super::StripDecoder) wraps; returns (output,
/// ended_at_boundary).
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

/// Decode one zlib strip with the public decoder; returns (output, ended at
/// strip boundary, Adler-32 of the output, trailer).
fn decode_with_strip_decoder(
    strip: &[u8],
    first: bool,
) -> Result<(Vec<u8>, bool, u32, Option<u32>), String> {
    let mut d = StripDecoder::new(strip, first, 64 * 1024);
    let mut out = Vec::new();
    while !d.is_done() {
        let got = d.fill().map_err(|e| format!("{e:?}"))?;
        let n = got.len();
        out.extend_from_slice(got);
        d.advance(n);
    }
    Ok((out, d.ended_at_strip_boundary(), d.adler32(), d.trailer()))
}

/// Every strip of a zlib stream, decoded alone by `StripDecoder` at every
/// level: content, boundary flags, and combined Adler-32 against the
/// trailer, including a wrong trailer (reported, never raised).
#[test]
fn strip_decoder_decodes_each_strip_and_reports_checksums() {
    let data = test_data(200_000);
    for (name, level) in levels() {
        for parts in [1usize, 3] {
            let strips = compress_strips(&data, parts, level);
            let per = data.len().div_ceil(parts);
            let mut z = StripCompressor::new(level).zlib_header().to_vec();
            let mut bounds = Vec::new();
            for s in &strips {
                bounds.push(z.len());
                z.extend_from_slice(s);
            }
            z.extend_from_slice(&adler32(1, &data).to_be_bytes());
            for bad in [false, true] {
                if bad {
                    let n = z.len();
                    z[n - 1] ^= 1;
                }
                let mut adler = 1;
                let mut trailer = None;
                for (k, chunk) in data.chunks(per).enumerate() {
                    let last = k + 1 == strips.len();
                    let start = if k == 0 { 0 } else { bounds[k] };
                    let end = if last { z.len() } else { bounds[k + 1] };
                    let (o, ended, a, t) = decode_with_strip_decoder(&z[start..end], k == 0)
                        .unwrap_or_else(|e| panic!("{name} parts={parts} strip {k}: {e}"));
                    assert_eq!(o, chunk, "{name} parts={parts} strip {k}");
                    assert_eq!(ended, !last, "{name} parts={parts} strip {k}");
                    assert_eq!(t.is_some(), last, "{name} parts={parts} strip {k}");
                    adler = adler32_combine(adler, a, o.len());
                    trailer = t;
                }
                assert_eq!(adler, adler32(1, &data), "{name} parts={parts}");
                assert_eq!(
                    trailer == Some(adler),
                    !bad,
                    "{name} parts={parts} bad={bad}"
                );
            }
        }
    }
}

/// A strip ending in a dangling stored header (the "ambiguous PNG"
/// construction) fails in `StripDecoder`; the raw-segment tests below cover
/// back-references and truncation in the machinery it wraps.
#[test]
fn strip_decoder_rejects_misframed_strips() {
    let data = test_data(10_000);
    let strips = compress_strips(&data, 2, CompressionLevel::new(6));
    let mut first = StripCompressor::new(CompressionLevel::new(6))
        .zlib_header()
        .to_vec();
    first.extend_from_slice(&strips[0]);
    assert!(decode_with_strip_decoder(&first, true).is_ok());
    first.extend_from_slice(&[0x00, 10, 0, !10u8, 0xff]);
    assert!(decode_with_strip_decoder(&first, true).is_err());
}

#[test]
fn segment_mode_off_still_reports_truncation() {
    let data = test_data(50_000);
    let segs = compress_strips(&data, 2, CompressionLevel::new(6));
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
    let mut segs = compress_strips(&data, 2, CompressionLevel::new(6));
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
        let segs = compress_strips(&data, 2, CompressionLevel::new(level));
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

/// Primed strips: new/png efforts 0-30, all C-compatible levels, and a
/// representative full-optimal level decode
/// back as one zlib stream; output doesn't depend on order or compressor
/// reuse; history beyond 32 KiB doesn't change anything; and on data with
/// repeats across strip ends, primed strips are smaller than independent
/// ones.
#[test]
fn strips_with_history() {
    let data = test_data(300_000);
    let mut all_levels = levels();
    for e in 0..=30 {
        for (name, level) in [
            (format!("new({e})"), CompressionLevel::new(e)),
            (format!("png({e})"), CompressionLevel::png(e)),
        ] {
            if !all_levels.iter().any(|(_, present)| *present == level) {
                all_levels.push((name, level));
            }
        }
    }
    for e in 0..=12 {
        all_levels.push((format!("libdeflate({e})"), CompressionLevel::libdeflate(e)));
    }
    for (name, level) in all_levels {
        for parts in [1usize, 3, 7] {
            let per = data.len().div_ceil(parts);
            let starts: Vec<usize> = (0..data.len()).step_by(per).collect();
            let n = starts.len();
            let compress = |k: usize, comp: &mut StripCompressor, window: usize| {
                let s = starts[k];
                let e = (s + per).min(data.len());
                let from = s.saturating_sub(window);
                let mut out = vec![0u8; StripCompressor::bound(e - s)];
                let len = comp
                    .compress_with_history(
                        &data[from..e],
                        s - from,
                        k + 1 == n,
                        &mut out,
                        Unstoppable,
                    )
                    .unwrap();
                out.truncate(len);
                out
            };
            // Forward with one compressor; backward with fresh ones; and
            // with the whole preceding image as history.
            let mut one = StripCompressor::new(level);
            let fwd: Vec<Vec<u8>> = (0..n).map(|k| compress(k, &mut one, 32 * 1024)).collect();
            let back: Vec<Vec<u8>> = (0..n)
                .rev()
                .map(|k| compress(k, &mut StripCompressor::new(level), 32 * 1024))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let long: Vec<Vec<u8>> = (0..n).map(|k| compress(k, &mut one, usize::MAX)).collect();
            assert_eq!(fwd, back, "{name}, {parts} parts: order/reuse");
            assert_eq!(fwd, long, "{name}, {parts} parts: history beyond 32 KiB");

            let mut z = one.zlib_header().to_vec();
            for s in &fwd {
                z.extend_from_slice(s);
            }
            z.extend_from_slice(&adler32(1, &data).to_be_bytes());
            let mut back_out = vec![0u8; data.len()];
            let r = Decompressor::new()
                .zlib_decompress(&z, &mut back_out, Unstoppable)
                .unwrap_or_else(|e| panic!("{name}, {parts} parts: {e:?}"));
            assert_eq!(r.output_written, data.len());
            assert!(back_out == data, "{name}, {parts} parts: content");
            let mut reference = vec![0u8; data.len()];
            let written = libdeflater::Decompressor::new()
                .zlib_decompress(&z, &mut reference)
                .unwrap();
            assert_eq!(written, data.len(), "{name}, {parts}: C length");
            assert_eq!(reference, data, "{name}, {parts}: C content");
        }
    }

    // Repeats that straddle strip ends: history makes strips smaller.
    let block: Vec<u8> = test_data(20_000);
    let rep: Vec<u8> = block.iter().cycle().take(200_000).copied().collect();
    let level = CompressionLevel::png(10);
    let independent: usize = compress_strips(&rep, 5, level).iter().map(Vec::len).sum();
    let per = rep.len().div_ceil(5);
    let mut comp = StripCompressor::new(level);
    let primed: usize = (0..5)
        .map(|k| {
            let s = k * per;
            let e = (s + per).min(rep.len());
            let mut out = vec![0u8; StripCompressor::bound(e - s)];
            comp.compress_with_history(&rep[..e], s, k == 4, &mut out, Unstoppable)
                .unwrap()
        })
        .sum();
    assert!(
        primed < independent,
        "primed {primed} vs independent {independent}"
    );
}

/// Empty/short tails and dictionary truncation, including reuse after overflow.
#[test]
fn history_boundaries_and_short_output_recovery() {
    let data = test_data(33_027);
    for level in [
        CompressionLevel::new(1),
        CompressionLevel::png(1),
        CompressionLevel::png(4),
        CompressionLevel::png(10),
        CompressionLevel::png(19),
        CompressionLevel::png(27),
        CompressionLevel::png(30),
        CompressionLevel::libdeflate(12),
        CompressionLevel::new(31),
    ] {
        for history in [0usize, 1, 7, 32767, 32768, 32769] {
            for tail in [0usize, 1, 7, 258] {
                let input = &data[..history + tail];
                let mut reused = StripCompressor::new(level);
                assert!(
                    reused
                        .compress_with_history(input, history, true, &mut [], Unstoppable)
                        .is_err()
                );
                let mut output = vec![0; StripCompressor::bound(tail)];
                let n = reused
                    .compress_with_history(input, history, true, &mut output, Unstoppable)
                    .unwrap();
                output.truncate(n);
                let mut fresh = StripCompressor::new(level);
                let mut expected = vec![0; StripCompressor::bound(tail)];
                let n = fresh
                    .compress_with_history(input, history, true, &mut expected, Unstoppable)
                    .unwrap();
                assert_eq!(
                    output,
                    expected[..n],
                    "{level:?}, history {history}, tail {tail}"
                );
                let mut prefix = vec![0; StripCompressor::bound(history)];
                let n = fresh
                    .compress(&input[..history], false, &mut prefix, Unstoppable)
                    .unwrap();
                let mut zlib = fresh.zlib_header().to_vec();
                zlib.extend_from_slice(&prefix[..n]);
                zlib.extend_from_slice(&output);
                zlib.extend_from_slice(&adler32(1, input).to_be_bytes());
                assert_eq!(
                    miniz_oxide::inflate::decompress_to_vec_zlib(&zlib).unwrap(),
                    input,
                    "{level:?}, history {history}, tail {tail}"
                );
            }
        }
    }
}

#[test]
fn history_reuse_after_cancellation() {
    struct Cancel;
    impl enough::Stop for Cancel {
        fn check(&self) -> Result<(), enough::StopReason> {
            Err(enough::StopReason::Cancelled)
        }
    }
    let data = test_data(80_000);
    for e in [1, 4, 10, 19, 27, 30] {
        let level = CompressionLevel::png(e);
        let mut reused = StripCompressor::new(level);
        let mut actual = vec![0; StripCompressor::bound(data.len() - 32768)];
        assert!(matches!(
            reused.compress_with_history(&data, 32768, true, &mut actual, Cancel),
            Err(crate::CompressionError::Stopped(
                enough::StopReason::Cancelled
            ))
        ));
        let n = reused
            .compress_with_history(&data, 32768, true, &mut actual, Unstoppable)
            .unwrap();
        actual.truncate(n);
        let mut expected = vec![0; StripCompressor::bound(data.len() - 32768)];
        let n = StripCompressor::new(level)
            .compress_with_history(&data, 32768, true, &mut expected, Unstoppable)
            .unwrap();
        assert_eq!(actual, expected[..n], "png({e}) after cancellation");
    }
}

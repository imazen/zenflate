// Differential check of the count-only scan, shared by the `fuzz_scan` target,
// `tests/fuzz_regression.rs` and `tests/scan.rs` (included with `include!`).
//
// The scan must agree with the one-shot decoder on every stream: same input
// consumed and output length, or the same error. A scan stopped at `k` output
// bytes must name the shortest input prefix from which miniz_oxide, told that
// more input follows, produces `k` bytes. (A decoder at end of input pads with
// zero bits, so it can produce `k` bytes one byte early when the bits it lacks
// happen to be zero; a decoder reading on, as in a PNG, reads the real ones.)

use zenflate::{Decompressor, Unstoppable, deflate_scan, zlib_scan};

const SCAN_LIMIT: usize = 1 << 20;

/// Bytes miniz_oxide produces from `prefix` when told more input follows (so it never
/// pads with zero bits): an independent count of what a prefix can yield.
fn miniz_produced(prefix: &[u8], cap: usize) -> usize {
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags::*};
    let mut st = DecompressorOxide::new();
    let mut out = vec![0u8; cap];
    let flags = TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_HAS_MORE_INPUT;
    decompress(&mut st, prefix, &mut out, 0, flags).2
}

/// Scan vs one-shot decode of `stream` as raw DEFLATE (and as zlib); then, when it
/// decodes, the stop-at positions for the given output counts.
pub fn check_scan(stream: &[u8], stops: &[usize]) {
    let mut out = vec![0u8; SCAN_LIMIT];
    let one = Decompressor::new().deflate_decompress(stream, &mut out, Unstoppable);
    let scan = deflate_scan(stream, None, Unstoppable);
    // miniz_oxide is an oracle only for streams it decodes too (the two disagree on a
    // few invalid inputs; `inflate_diff_check.rs` compares them the same way).
    let mut miniz_ok = false;
    match (&one, &scan) {
        (Ok(r), Ok(s)) => {
            assert_eq!(
                (s.input_consumed, s.output_len, s.stopped),
                (r.input_consumed, r.output_written, false),
                "scan and decode disagree"
            );
            // miniz_oxide: an independent count of the input consumed.
            let mut st = miniz_oxide::inflate::core::DecompressorOxide::new();
            let mut big = vec![0u8; r.output_written.max(1)];
            let (status, used, _) = miniz_oxide::inflate::core::decompress(
                &mut st,
                stream,
                &mut big,
                0,
                miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            miniz_ok = status == miniz_oxide::inflate::TINFLStatus::Done;
            if miniz_ok {
                assert_eq!(used, s.input_consumed, "miniz_oxide consumed differs");
            }
        }
        (Err(e), Err(s)) => assert_eq!(*e, s.error, "scan and decode fail differently"),
        (Err(zenflate::DecompressionError::InsufficientSpace), Ok(s)) => {
            assert!(s.output_len > SCAN_LIMIT)
        }
        _ => panic!("scan {scan:?} vs decode {one:?}"),
    }
    // zlib framing: same verdict as zlib_decompress (checksum not verified).
    let mut z = vec![0x78, 0x01];
    z.extend_from_slice(stream);
    z.extend_from_slice(&[0, 0, 0, 0]);
    let zone = Decompressor::new()
        .with_skip_checksum(true)
        .zlib_decompress(&z, &mut out, Unstoppable);
    let zscan = zlib_scan(&z, None, Unstoppable);
    if let (Ok(r), Ok(s)) = (&zone, &zscan) {
        assert_eq!((s.input_consumed, s.output_len), (r.input_consumed, r.output_written));
    }
    let Ok(s) = scan else { return };
    for &k in stops {
        if k == 0 || k > s.output_len || s.output_len > SCAN_LIMIT {
            continue;
        }
        let at = deflate_scan(stream, Some(k), Unstoppable).expect("prefix of a valid stream");
        assert!(at.stopped && at.output_len >= k && at.input_consumed <= s.input_consumed);
        let p = at.input_consumed;
        if !miniz_ok {
            continue;
        }
        let cap = at.output_len + 300;
        assert!(miniz_produced(&stream[..p], cap) >= k, "prefix {p} yields fewer than {k}");
        assert!(miniz_produced(&stream[..p - 1], cap) < k, "prefix {} already yields {k}", p - 1);
    }
}

/// Fuzz entry: byte 0 picks the stop points, the rest is raw DEFLATE.
pub fn check(data: &[u8]) {
    let Some((&sel, stream)) = data.split_first() else { return };
    let stops = [1, 2, sel as usize + 1, (sel as usize) << 6, 1 << 12];
    check_scan(stream, &stops);
}

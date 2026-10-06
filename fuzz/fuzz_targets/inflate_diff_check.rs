// Differential inflate check shared by the `fuzz_inflate_diff` target and
// `tests/fuzz_regression.rs` (included with `include!`).
//
// Input: byte 0 picks the streaming source's chunk size, byte 1 the streaming
// buffer capacity, and the rest is raw DEFLATE. zenflate one-shot, zenflate
// streaming and miniz_oxide decode it; the two zenflate decoders must agree
// exactly (both fail, or both produce the same bytes), and whenever zenflate
// and miniz_oxide both succeed their bytes must match.

use zenflate::{Decompressor, InputSource, StreamDecompressor, Unstoppable};

const LIMIT: usize = 256 * 1024;

/// Hands out at most `chunk` bytes per `fill_buf`, like PNG IDAT chunks.
struct ChunkedSource<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl InputSource for ChunkedSource<'_> {
    type Error = core::convert::Infallible;
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        Ok(&self.data[..self.data.len().min(self.chunk)])
    }
    fn consume(&mut self, n: usize) {
        self.data = &self.data[n..];
    }
}

fn stream_decode(stream: &[u8], chunk: usize, capacity: usize) -> Option<Vec<u8>> {
    let src = ChunkedSource {
        data: stream,
        chunk,
    };
    let mut d = StreamDecompressor::deflate(src, capacity).with_max_output_size(Some(LIMIT));
    let mut out = Vec::new();
    while !d.is_done() {
        let got = d.fill().ok()?;
        let n = got.len();
        out.extend_from_slice(got);
        d.advance(n);
    }
    Some(out)
}

pub fn check(data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let chunk = [1, 2, 3, 7, 8, 13, 64, 509, 4096, usize::MAX][data[0] as usize % 10];
    let capacity = [1, 2, 3, 5, 64, 300, 4096, 1 << 16][data[1] as usize % 8];
    let stream = &data[2..];

    let mut out = vec![0u8; LIMIT];
    let one = Decompressor::new()
        .deflate_decompress(stream, &mut out, Unstoppable)
        .ok()
        .map(|r| out[..r.output_written].to_vec());
    let streamed = stream_decode(stream, chunk, capacity);
    match (&one, &streamed) {
        (Some(a), Some(b)) => assert!(a == b, "one-shot and streaming decode differ"),
        (None, None) => {}
        _ => panic!(
            "one-shot ok={} but streaming ok={} (chunk {chunk}, capacity {capacity})",
            one.is_some(),
            streamed.is_some()
        ),
    }
    if let (Some(a), Ok(m)) = (
        &one,
        miniz_oxide::inflate::decompress_to_vec_with_limit(stream, LIMIT),
    ) {
        assert!(*a == m, "zenflate and miniz_oxide decode differ");
    }
}

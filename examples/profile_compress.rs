fn make_mixed(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut state: u32 = 0xDEAD_BEEF;
    let mut i = 0;
    while i < size {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let byte = (state >> 16) as u8;
        if i % 256 == 0 && i + 32 <= size {
            data.extend(std::iter::repeat_n(byte, 32));
            i += 32;
        } else {
            data.push(byte);
            i += 1;
        }
    }
    data.truncate(size);
    data
}

fn main() {
    let level: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(if level >= 10 { 10 } else { 500 });

    let data = make_mixed(1_000_000);
    let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
    let mut out = vec![0u8; bound];

    // Run many iterations for profiling
    for _ in 0..iters {
        std::hint::black_box(
            compressor
                .deflate_compress(&data, &mut out, zenflate::Unstoppable)
                .unwrap(),
        );
    }
}

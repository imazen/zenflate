/// Heap allocation comparison: zenflate vs libdeflate C.
/// Run with: heaptrack cargo run --release --example heap_compare -- <level>

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
        .unwrap_or(6);

    let data = make_mixed(1_000_000);

    // --- zenflate ---
    let mut zc = zenflate::Compressor::new(zenflate::CompressionLevel::new(level));
    let bound = zenflate::Compressor::deflate_compress_bound(data.len());
    let mut zout = vec![0u8; bound];
    let zsize = zc.deflate_compress(&data, &mut zout).unwrap();

    // --- libdeflate C ---
    let mut lc = libdeflater::Compressor::new(
        libdeflater::CompressionLvl::new(level as i32).unwrap(),
    );
    let lbound = lc.deflate_compress_bound(data.len());
    let mut lout = vec![0u8; lbound];
    let lsize = lc.deflate_compress(&data, &mut lout).unwrap();

    eprintln!("Level {level}: zenflate={zsize} bytes, libdeflate={lsize} bytes");
}

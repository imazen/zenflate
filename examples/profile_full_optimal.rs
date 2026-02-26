/// Profile zenflate FullOptimal compression for callgrind analysis.
///
/// Usage: cargo run --release --example profile_full_optimal [-- /path/to/filtered_data.bin]
///
/// Without arguments, compresses zenzop's profile_squeeze test data (real PNG).

fn main() {
    let path = std::env::args().nth(1);

    let data = if let Some(p) = path {
        std::fs::read(&p).unwrap()
    } else {
        // Use the same real PNG data that zenzop's profile_squeeze uses
        let png_path = "/home/lilith/work/codec-corpus/clic2025-1024/0d154749c7771f58e89ad343653ec4e20d6f037da829f47f5598e5d0a4ab61f0.png";
        let png = std::fs::read(png_path).unwrap();
        let idat = extract_idat(&png);
        decompress_zlib(&idat)
    };

    eprintln!("Input: {} bytes", data.len());

    let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(31));
    let bound = zenflate::Compressor::zlib_compress_bound(data.len());
    let mut output = vec![0u8; bound];
    let len = compressor
        .zlib_compress(&data, &mut output, &zenflate::Unstoppable)
        .unwrap();

    eprintln!("Output: {} bytes", len);
}

fn extract_idat(png: &[u8]) -> Vec<u8> {
    let mut idat = Vec::new();
    let mut pos = 8;
    while pos + 12 <= png.len() {
        let len =
            u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let chunk_type = &png[pos + 4..pos + 8];
        if chunk_type == b"IDAT" {
            idat.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        }
        pos += 12 + len;
    }
    idat
}

fn decompress_zlib(data: &[u8]) -> Vec<u8> {
    let mut d = zenflate::Decompressor::new();
    let mut out = vec![0u8; data.len() * 10];
    loop {
        match d.zlib_decompress(data, &mut out, &zenflate::Unstoppable) {
            Ok(o) => {
                out.truncate(o.output_written);
                return out;
            }
            Err(zenflate::DecompressionError::InsufficientSpace) => {
                out.resize(out.len() * 2, 0);
            }
            Err(e) => panic!("{e}"),
        }
    }
}

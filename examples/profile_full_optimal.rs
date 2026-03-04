/// Profile zenflate FullOptimal compression for callgrind analysis.
///
/// Usage: cargo run --release --example profile_full_optimal [-- /path/to/filtered_data.bin]
///
/// Without arguments, compresses zenzop's profile_squeeze test data (real PNG).

fn main() {
    let arg1 = std::env::args().nth(1);
    // If arg1 is a pure number, it's the effort level, not a path
    let path = arg1.filter(|s| s.parse::<u32>().is_err());

    let data = if let Some(p) = path {
        std::fs::read(&p).unwrap()
    } else {
        // Use the same real PNG data that zenzop's profile_squeeze uses
        let corpus = std::env::var("CODEC_CORPUS_DIR").unwrap_or_else(|_| {
            let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
            parent.join("codec-corpus").to_string_lossy().into_owned()
        });
        let png_path = format!(
            "{corpus}/clic2025-1024/0d154749c7771f58e89ad343653ec4e20d6f037da829f47f5598e5d0a4ab61f0.png"
        );
        let png = std::fs::read(&png_path)
            .unwrap_or_else(|e| panic!("Failed to read {png_path}: {e}. Set CODEC_CORPUS_DIR."));
        let idat = extract_idat(&png);
        decompress_zlib(&idat)
    };

    eprintln!("Input: {} bytes", data.len());

    // effort = iterations + 16 (E31=15i, E76=60i)
    // Use last numeric arg, or default to 31
    let effort: u32 = std::env::args()
        .skip(1)
        .filter_map(|s| s.parse::<u32>().ok())
        .last()
        .unwrap_or(31);

    let mut compressor = zenflate::Compressor::new(zenflate::CompressionLevel::new(effort));
    let bound = zenflate::Compressor::zlib_compress_bound(data.len());
    let mut output = vec![0u8; bound];
    let len = compressor
        .zlib_compress(&data, &mut output, &zenflate::Unstoppable)
        .unwrap();

    eprintln!("Effort {effort} ({}i): {} bytes", effort - 16, len);
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

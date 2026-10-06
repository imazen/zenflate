//! Ratio cost of chunked ("streaming") compression: each chunk compressed with
//! the previous 32 KiB as dictionary, ending on a block boundary. Measured via
//! gzip_compress_parallel (same chunk primitive) vs whole-buffer gzip_compress.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

include!("../helpers.rs");

fn t_paeth(img: &Image) -> Vec<u8> {
    let (w, h, ch) = (img.w, img.h, img.ch);
    let rb = w * ch;
    let mut out = Vec::with_capacity(h * (rb + 1));
    let zero = vec![0u8; rb];
    for y in 0..h {
        let cur = &img.px[y * rb..y * rb + rb];
        let prev: &[u8] = if y == 0 { &zero } else { &img.px[(y - 1) * rb..y * rb] };
        out.push(4);
        for i in 0..rb {
            let a = if i >= ch { cur[i - ch] } else { 0 };
            let c = if i >= ch { prev[i - ch] } else { 0 };
            out.push(cur[i].wrapping_sub(paeth(a, prev[i], c)));
        }
    }
    out
}

fn main() {
    let dir = std::env::var("IMG_CORPUS_DIR").unwrap();
    let mut paths = Vec::new();
    collect_pngs(Path::new(&dir), &mut paths);
    paths.sort();
    let chunk_sizes = [64 * 1024usize, 256 * 1024, 1024 * 1024];
    let mut levels: Vec<(String, zenflate::CompressionLevel)> = Vec::new();
    for e in [1u32, 5, 10, 15, 22, 23, 26, 30] {
        levels.push((format!("new({e})"), zenflate::CompressionLevel::new(e)));
    }
    // (transform, level) -> [whole, chunk64, chunk256, chunk1M]
    let mut acc: BTreeMap<(String, String), [u64; 4]> = BTreeMap::new();
    let mut n_img = 0;
    for p in &paths {
        let Some(img) = decode_png(p) else { continue };
        let img = maybe_crop(&img, 1_000_000);
        n_img += 1;
        for (tname, data) in [("adaptive", t_png_filter(&img)), ("paeth", t_paeth(&img))] {
            for (lname, level) in &levels {
                let mut c = zenflate::Compressor::new(*level);
                let mut out = vec![0u8; zenflate::Compressor::gzip_compress_bound(data.len()) + 64 * 64];
                let whole = c.gzip_compress(&data, &mut out, zenflate::Unstoppable).unwrap();
                let mut sizes = [whole as u64, 0, 0, 0];
                for (i, cs) in chunk_sizes.iter().enumerate() {
                    let threads = data.len().div_ceil(*cs).max(2);
                    let n = c.gzip_compress_parallel(&data, &mut out, threads, zenflate::Unstoppable).unwrap();
                    let mut back = vec![0u8; data.len()];
                    let r = zenflate::Decompressor::new().gzip_decompress(&out[..n], &mut back, zenflate::Unstoppable).unwrap();
                    assert!(r.output_written == data.len() && back == data, "{lname} chunked roundtrip");
                    sizes[i + 1] = n as u64;
                }
                let a = acc.entry((tname.to_string(), lname.clone())).or_insert([0; 4]);
                for i in 0..4 {
                    a[i] += sizes[i];
                }
            }
        }
        eprintln!("{}", p.file_name().unwrap().to_string_lossy());
    }
    println!("# {n_img} images, 1 MP crops; cost = chunked size / whole size - 1");
    println!("transform,level,whole_bytes,cost_64k_pct,cost_256k_pct,cost_1m_pct");
    for ((t, l), a) in &acc {
        let pct = |x: u64| (x as f64 / a[0] as f64 - 1.0) * 100.0;
        println!("{t},{l},{},{:.3},{:.3},{:.3}", a[0], pct(a[1]), pct(a[2]), pct(a[3]));
    }
}

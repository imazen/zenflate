//! PNG-mode benchmark: `CompressionLevel::png(1..)` vs `new()` efforts,
//! fdeflate (main) and zlib-rs on PNG IDAT-shaped streams.
//!
//! Each image under IMG_CORPUS_DIR is center-cropped to CROP_PX pixels (set it
//! high to use images as they are), then turned into IDAT-shaped streams by the
//! TRANSFORMS listed (png_adaptive = per-row min-sum-of-absolute-residuals filter,
//! png_none / png_sub / png_up / png_paeth = one filter on every row).
//!
//! Every output is decoded by zenflate and compared byte for byte, then timed
//! interleaved: each of REPS rounds runs every codec once; the median per
//! (image, codec) counts. Compressors and output buffers are reused across
//! calls. Aggregate MiB/s = total input / summed medians.
//!
//! Env: IMG_CORPUS_DIR, IMG_PER_CLASS, CROP_PX, REPS, TRANSFORMS, CODEC_FILTER
//! (comma-separated name prefixes), PNG_EFFORTS, ZF_EFFORTS, PER_IMAGE (emit
//! SIZE and TIME lines on stderr for validation/analyze.py).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

include!("helpers.rs");

fn class_of(path: &Path) -> String {
    let d = path.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    let n: u32 = d.split(['-', '_']).next().and_then(|s| s.parse().ok()).unwrap_or(0);
    match n {
        2400 => "texture",
        2200 => "render",
        1000..=3999 => "photo",
        5000..=5999 => "document",
        6000..=6999 => "scan",
        7000..=7999 => "plot",
        8000..=8999 => "screenshot",
        9000..=9999 => "clipart",
        _ => "other",
    }
    .to_string()
}

/// PNG "None" filter on every row (what fast encoders / palette images often feed).
fn t_png_none(img: &Image) -> Vec<u8> {
    let rb = img.w * img.ch;
    let mut out = Vec::with_capacity(img.h * (rb + 1));
    for row in img.px.chunks(rb) {
        out.push(0);
        out.extend_from_slice(row);
    }
    out
}

/// A codec writes into a reusable output buffer and returns the output size.
/// Compressors and buffers live across calls, so allocation stays out of the
/// timed region for every contender.
/// Every row filtered with one fixed PNG filter (1=Sub, 2=Up, 4=Paeth).
fn t_png_fixed(img: &Image, filter: u8) -> Vec<u8> {
    let (w, h, ch) = (img.w, img.h, img.ch);
    let rb = w * ch;
    let mut out = Vec::with_capacity(h * (rb + 1));
    let zero = vec![0u8; rb];
    for y in 0..h {
        let cur = &img.px[y * rb..y * rb + rb];
        let prev: &[u8] = if y == 0 { &zero } else { &img.px[(y - 1) * rb..y * rb] };
        out.push(filter);
        for i in 0..rb {
            let a = if i >= ch { cur[i - ch] } else { 0 };
            let b = prev[i];
            let c = if i >= ch { prev[i - ch] } else { 0 };
            out.push(match filter {
                1 => cur[i].wrapping_sub(a),
                2 => cur[i].wrapping_sub(b),
                _ => cur[i].wrapping_sub(paeth(a, b, c)),
            });
        }
    }
    out
}
fn t_sub(img: &Image) -> Vec<u8> { t_png_fixed(img, 1) }
fn t_up(img: &Image) -> Vec<u8> { t_png_fixed(img, 2) }
fn t_paeth(img: &Image) -> Vec<u8> { t_png_fixed(img, 4) }

type CodecFn = Box<dyn FnMut(&[u8], &mut Vec<u8>) -> usize>;
type Codec = (&'static str, CodecFn);

fn zf_level(level: zenflate::CompressionLevel) -> CodecFn {
    let mut c = zenflate::Compressor::new(level);
    Box::new(move |d: &[u8], out: &mut Vec<u8>| {
        let bound = zenflate::Compressor::zlib_compress_bound(d.len());
        if out.len() < bound {
            out.resize(bound, 0);
        }
        c.zlib_compress(d, &mut out[..], zenflate::Unstoppable).unwrap()
    })
}

fn zlrs(level: i32, strategy: zlib_rs::Strategy) -> CodecFn {
    Box::new(move |d: &[u8], out: &mut Vec<u8>| {
        let config = zlib_rs::DeflateConfig {
            level,
            method: zlib_rs::Method::Deflated,
            window_bits: 15,
            mem_level: 8,
            strategy,
        };
        let need = d.len() * 2 + 4096;
        if out.len() < need {
            out.resize(need, 0);
        }
        let (c, rc) = zlib_rs::compress_slice(&mut out[..], d, config);
        assert_eq!(rc, zlib_rs::ReturnCode::Ok);
        c.len()
    })
}

fn codecs() -> Vec<Codec> {
    let mut v: Vec<Codec> = vec![
        (
            "ref-memcpy",
            Box::new(|d: &[u8], out: &mut Vec<u8>| {
                if out.len() < d.len() {
                    out.resize(d.len(), 0);
                }
                out[..d.len()].copy_from_slice(d);
                d.len()
            }),
        ),
        ("ref-store", zf_level(zenflate::CompressionLevel::new(0))),
        (
            "fd-ultrafast",
            Box::new(|d: &[u8], out: &mut Vec<u8>| {
                out.clear();
                let mut c = fdeflate::UltraFastCompressor::new(&mut *out).unwrap();
                c.write_data(d).unwrap();
                c.finish().unwrap();
                out.len()
            }),
        ),
        (
            "fd-rle",
            Box::new(|d: &[u8], out: &mut Vec<u8>| {
                out.clear();
                let mut c = fdeflate::Compressor::new_rle(&mut *out, true).unwrap();
                c.write_data(d).unwrap();
                c.finish().unwrap();
                out.len()
            }),
        ),
    ];
    for lvl in [1u8, 2, 3, 6] {
        let name: &'static str = Box::leak(format!("fd-L{lvl}").into_boxed_str());
        v.push((
            name,
            Box::new(move |d: &[u8], out: &mut Vec<u8>| {
                out.clear();
                let mut c = fdeflate::Compressor::new(&mut *out, lvl, true).unwrap();
                c.write_data(d).unwrap();
                c.finish().unwrap();
                out.len()
            }),
        ));
    }
    v.push(("zlibrs-rle", zlrs(6, zlib_rs::Strategy::Rle)));
    v.push(("zlibrs-L1", zlrs(1, zlib_rs::Strategy::Default)));
    for e in std::env::var("PNG_EFFORTS").unwrap_or("1,2,3,4,5,6,7,8,9,10,11,12,13,14".into()).split(',').filter(|s| !s.is_empty()) {
        let e: u32 = e.parse().unwrap();
        let name: &'static str = Box::leak(format!("png-e{e:02}").into_boxed_str());
        v.push((name, zf_level(zenflate::CompressionLevel::png(e))));
    }
    for e in std::env::var("ZF_EFFORTS").unwrap_or("1,5,10,15".into()).split(',').filter(|s| !s.is_empty()) {
        let e: u32 = e.parse().unwrap();
        let name: &'static str = Box::leak(format!("zf-e{e}").into_boxed_str());
        v.push((name, zf_level(zenflate::CompressionLevel::new(e))));
    }
    v
}

#[derive(Default, Clone)]
struct Acc {
    n: usize,
    in_b: u64,
    out_b: u64,
    secs: f64,
}

fn main() {
    let dir = std::env::var("IMG_CORPUS_DIR")
        .unwrap_or("/home/lilith/work/zen/imazen-26-png-v3/png-v3".into());
    let per_class: usize = std::env::var("IMG_PER_CLASS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let cap: usize = std::env::var("CROP_PX").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let reps: usize = std::env::var("REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    let filter_only = std::env::var("CODEC_FILTER").ok();
    let mut paths = Vec::new();
    collect_pngs(Path::new(&dir), &mut paths);
    paths.sort();
    let mut by_class: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for p in paths {
        let c = class_of(&p);
        let e = by_class.entry(c).or_default();
        if e.len() < per_class * 3 {
            e.push(p);
        }
    }
    let mut codecs: Vec<Codec> = codecs()
        .into_iter()
        .filter(|(n, _)| filter_only.as_ref().is_none_or(|f| f.split(',').any(|x| n.starts_with(x))))
        .collect();
    let mut outs: Vec<Vec<u8>> = (0..codecs.len()).map(|_| Vec::new()).collect();
    let all_transforms: [(&str, fn(&Image) -> Vec<u8>); 5] = [
        ("png_adaptive", t_png_filter),
        ("png_none", t_png_none),
        ("png_sub", t_sub),
        ("png_up", t_up),
        ("png_paeth", t_paeth),
    ];
    let want = std::env::var("TRANSFORMS").unwrap_or("png_adaptive,png_none".into());
    let transforms: Vec<(&str, fn(&Image) -> Vec<u8>)> = all_transforms
        .into_iter()
        .filter(|(n, _)| want.split(',').any(|w| w == *n))
        .collect();
    // acc[(transform, class, codec)]
    let mut acc: BTreeMap<(String, String, String), Acc> = BTreeMap::new();
    let mut dec = zenflate::Decompressor::new();
    for (class, ps) in &by_class {
        let mut used = 0;
        for p in ps {
            if used >= per_class {
                break;
            }
            let Some(img) = decode_png(p) else { continue };
            used += 1;
            let img = maybe_crop(&img, cap);
            eprintln!("{class} {} {}x{}x{}", p.file_name().unwrap().to_string_lossy(), img.w, img.h, img.ch);
            for (tname, t) in &transforms {
                let data = t(&img);
                // Pass 1: verify every codec and record sizes.
                let mut sizes = vec![0usize; codecs.len()];
                for (ci, (cname, f)) in codecs.iter_mut().enumerate() {
                    let out = &mut outs[ci];
                    let n = f(&data, out);
                    if !cname.starts_with("ref-memcpy") {
                        let mut back = vec![0u8; data.len()];
                        let r = if cname.starts_with("par") {
                            dec.gzip_decompress(&out[..n], &mut back, zenflate::Unstoppable)
                        } else {
                            dec.zlib_decompress(&out[..n], &mut back, zenflate::Unstoppable)
                        }
                        .unwrap_or_else(|e| panic!("{cname} roundtrip decode failed: {e:?}"));
                        assert!(r.output_written == data.len() && back == data, "{cname} roundtrip mismatch");
                    }
                    sizes[ci] = n;
                }
                // Pass 2: interleaved timing (each round runs every codec once).
                let mut times: Vec<Vec<f64>> = vec![Vec::with_capacity(reps); codecs.len()];
                for _ in 0..reps {
                    for (ci, (_, f)) in codecs.iter_mut().enumerate() {
                        let out = &mut outs[ci];
                        let t0 = Instant::now();
                        std::hint::black_box(f(std::hint::black_box(&data), out));
                        times[ci].push(t0.elapsed().as_secs_f64());
                    }
                }
                for (ci, (cname, _)) in codecs.iter().enumerate() {
                    let mut t = times[ci].clone();
                    t.sort_by(f64::total_cmp);
                    let s = t[t.len() / 2];
                    let c_len = sizes[ci];
                    if std::env::var("PER_IMAGE").is_ok() {
                        let img_name = p.file_name().unwrap().to_string_lossy();
                        eprintln!("SIZE,{tname},{img_name},{cname},{c_len}");
                        eprintln!("TIME,{tname},{img_name},{cname},{s:.9},{}", data.len());
                    }
                    for cl in [class.clone(), "*ALL".to_string()] {
                        let a = acc.entry((tname.to_string(), cl, cname.to_string())).or_default();
                        a.n += 1;
                        a.in_b += data.len() as u64;
                        a.out_b += c_len as u64;
                        a.secs += s;
                    }
                }
            }
        }
    }
    println!("transform,class,codec,n,in_bytes,out_bytes,ratio,mib_s");
    for ((t, cl, c), a) in &acc {
        println!(
            "{t},{cl},{c},{},{},{},{:.4},{:.1}",
            a.n,
            a.in_b,
            a.out_b,
            a.in_b as f64 / a.out_b as f64,
            a.in_b as f64 / (1 << 20) as f64 / a.secs
        );
    }
}

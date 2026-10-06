//! Compression ladders on identical PNG filtered bytes: zenflate `new(e)`,
//! `png(e)` and `libdeflate(n)` against miniz_oxide 1-9, zlib-rs 1-9,
//! libdeflate (C) 1-12 and fdeflate's ultra-fast compressor.
//!
//! Input: PNG files; each one's IDAT stream is inflated to recover the
//! filtered scanlines exactly as stored (filter bytes included), and every arm
//! compresses those same bytes to zlib. Output is size and time per arm per
//! image (TSV) plus corpus totals with the Pareto front marked. Time is the
//! median over rounds of one compression with a reused compressor where the
//! library allows it (miniz_oxide's `compress_to_vec_zlib` allocates per call,
//! as image-png's flate2 path does).
//!
//! ```text
//! cargo run --release --example png_ladder_pareto -- [DIR_OR_PNG ...] [--rounds N]
//!     [--filter SUBSTR] [--tsv OUT.tsv] [--arms NAME,NAME]
//! ```
//! Default input: `~/tmp/mtpng/bench_in` (zenpng's `scripts/vs_png_inputs.sh`).

use std::path::PathBuf;
use std::time::Instant;

use zenflate::{CompressionLevel, Compressor, Unstoppable};

fn idat_filtered(path: &std::path::Path) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut pos = 8;
    let mut z = Vec::new();
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let ty = &data[pos + 4..pos + 8];
        if ty == b"IDAT" {
            z.extend_from_slice(data.get(pos + 8..pos + 8 + len)?);
        }
        if ty == b"IEND" {
            break;
        }
        pos += 12 + len;
    }
    miniz_oxide::inflate::decompress_to_vec_zlib(&z).ok()
}

enum Kind {
    Zen(CompressionLevel),
    Miniz(u8),
    ZlibRs(u32),
    Libdeflate(i32),
    FdeflateUltra,
}

struct Arm {
    name: String,
    kind: Kind,
}

fn arms() -> Vec<Arm> {
    let mut v = Vec::new();
    for e in 1u32..=19 {
        v.push(Arm {
            name: format!("zen_png{e}"),
            kind: Kind::Zen(CompressionLevel::png(e)),
        });
    }
    for e in [
        1u32, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24, 26, 30,
    ] {
        v.push(Arm {
            name: format!("zen_e{e}"),
            kind: Kind::Zen(CompressionLevel::new(e)),
        });
    }
    for l in 1..=9 {
        v.push(Arm {
            name: format!("miniz_{l}"),
            kind: Kind::Miniz(l),
        });
        v.push(Arm {
            name: format!("zlibrs_{l}"),
            kind: Kind::ZlibRs(l as u32),
        });
    }
    for l in 1..=12 {
        v.push(Arm {
            name: format!("libdeflate_{l}"),
            kind: Kind::Libdeflate(l),
        });
    }
    v.push(Arm {
        name: "fdeflate_ultra".into(),
        kind: Kind::FdeflateUltra,
    });
    v
}

/// One compression; returns compressed size.
fn run(
    kind: &Kind,
    input: &[u8],
    zen: &mut Option<Compressor>,
    ld: &mut Option<libdeflater::Compressor>,
    out: &mut Vec<u8>,
) -> usize {
    match kind {
        Kind::Zen(level) => {
            let c = zen.get_or_insert_with(|| Compressor::new(*level));
            let bound = Compressor::zlib_compress_bound(input.len());
            if out.len() < bound {
                out.resize(bound, 0);
            }
            c.zlib_compress(input, out, Unstoppable).unwrap()
        }
        Kind::Miniz(l) => miniz_oxide::deflate::compress_to_vec_zlib(input, *l).len(),
        Kind::ZlibRs(l) => {
            use std::io::Write;
            out.clear();
            let mut e =
                flate2::write::ZlibEncoder::new(std::mem::take(out), flate2::Compression::new(*l));
            e.write_all(input).unwrap();
            *out = e.finish().unwrap();
            out.len()
        }
        Kind::Libdeflate(l) => {
            let c = ld.get_or_insert_with(|| {
                libdeflater::Compressor::new(libdeflater::CompressionLvl::new(*l).unwrap())
            });
            let bound = c.zlib_compress_bound(input.len());
            if out.len() < bound {
                out.resize(bound, 0);
            }
            c.zlib_compress(input, out).unwrap()
        }
        Kind::FdeflateUltra => fdeflate::compress_to_vec(input).len(),
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut rounds = 5usize;
    let mut filter = String::new();
    let mut tsv: Option<PathBuf> = None;
    let mut arm_filter: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rounds" => rounds = args.next().unwrap().parse().unwrap(),
            "--filter" => filter = args.next().unwrap(),
            "--tsv" => tsv = Some(args.next().unwrap().into()),
            "--arms" => arm_filter = args.next().unwrap().split(',').map(String::from).collect(),
            _ => inputs.push(a.into()),
        }
    }
    if inputs.is_empty() {
        inputs.push(PathBuf::from(std::env::var("HOME").unwrap()).join("tmp/mtpng/bench_in"));
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for i in &inputs {
        if i.is_dir() {
            let mut v: Vec<PathBuf> = std::fs::read_dir(i)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "png"))
                .collect();
            v.sort();
            files.extend(v);
        } else {
            files.push(i.clone());
        }
    }
    files.retain(|f| f.to_string_lossy().contains(&filter));
    let mut arms = arms();
    if !arm_filter.is_empty() {
        arms.retain(|a| arm_filter.contains(&a.name));
    }

    let mut tsv_out = String::from("image\tfiltered_bytes\tarm\tbytes\tmedian_us\n");
    let mut totals: Vec<(u64, f64)> = vec![(0, 0.0); arms.len()];
    let mut total_in = 0u64;
    let mut out = Vec::new();
    for f in &files {
        let Some(data) = idat_filtered(f) else {
            continue;
        };
        let name = f.file_stem().unwrap().to_string_lossy().into_owned();
        total_in += data.len() as u64;
        // Repeat small inputs so one sample is at least ~1 ms of work at 1 GB/s.
        let reps = (1_000_000 / data.len().max(1)).clamp(1, 1000);
        let mut zens: Vec<Option<Compressor>> = (0..arms.len()).map(|_| None).collect();
        let mut lds: Vec<Option<libdeflater::Compressor>> = (0..arms.len()).map(|_| None).collect();
        let mut sizes = vec![0usize; arms.len()];
        let mut times: Vec<Vec<f64>> = vec![Vec::new(); arms.len()];
        for _ in 0..rounds {
            for (k, a) in arms.iter().enumerate() {
                let s = Instant::now();
                for _ in 0..reps {
                    sizes[k] = run(&a.kind, &data, &mut zens[k], &mut lds[k], &mut out);
                }
                times[k].push(s.elapsed().as_secs_f64() * 1e6 / reps as f64);
            }
        }
        for (k, a) in arms.iter().enumerate() {
            let m = median(&mut times[k]);
            totals[k].0 += sizes[k] as u64;
            totals[k].1 += m;
            tsv_out.push_str(&format!(
                "{name}\t{}\t{}\t{}\t{m:.2}\n",
                data.len(),
                a.name,
                sizes[k]
            ));
        }
        eprintln!("{name}: done");
    }
    if let Some(p) = tsv {
        std::fs::write(&p, &tsv_out).unwrap();
    }
    // Pareto front over (total time, total bytes).
    let mut order: Vec<usize> = (0..arms.len()).collect();
    order.sort_by(|&a, &b| totals[a].1.partial_cmp(&totals[b].1).unwrap());
    let mut best = u64::MAX;
    println!(
        "{} images, {:.1} MB filtered",
        files.len(),
        total_in as f64 / 1e6
    );
    println!(
        "{:<16} {:>12} {:>9} {:>10} {:>9}  front",
        "arm", "bytes", "ratio", "ms total", "MB/s"
    );
    for &k in &order {
        let (b, t) = totals[k];
        let front = b < best;
        if front {
            best = b;
        }
        println!(
            "{:<16} {:>12} {:>9.4} {:>10.1} {:>9.0}  {}",
            arms[k].name,
            b,
            total_in as f64 / b as f64,
            t / 1000.0,
            total_in as f64 / t,
            if front { "*" } else { "" }
        );
    }
}

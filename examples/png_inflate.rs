//! Inflate-only speed on real PNG IDAT streams: zenflate one-shot, zenflate
//! streaming (driven the way zenpng drives it), fdeflate (image-png's
//! inflater) and libdeflate (C).
//!
//! The streaming arm feeds the zlib stream one IDAT chunk at a time through an
//! `InputSource`, with zenpng's buffer capacity (two rows, at least 256 KiB
//! unless the image is smaller), and consumes output a row at a time with
//! `peek`/`advance`. Checksums are skipped in every arm that allows it, as
//! zenpng and image-png do by default.
//!
//! Arms are interleaved per round; each arm's time is the median over rounds.
//!
//! ```text
//! cargo run --release --example png_inflate -- [DIR_OR_PNG ...] [--rounds N] [--filter SUBSTR]
//! ```
//! `--raw` treats every input as plain data instead of PNG: it is compressed
//! to zlib with `CompressionLevel::libdeflate(6)` (byte-identical to C
//! libdeflate level 6), split into 32 KiB "chunks", and consumed 4 KiB at a
//! time by the streaming arm (for Silesia/Canterbury-style corpora).
//!
//! `--profile zen1|zenS|fdef|libd` runs only that arm over the selected images
//! for `--secs S` seconds (default 5; for `perf record` or callgrind).
//! Default input: `~/tmp/mtpng/bench_in` (zenpng's `scripts/vs_png_inputs.sh` output).

use std::path::PathBuf;
use std::time::Instant;

use zenflate::{Decompressor, InputSource, StreamDecompressor, Unstoppable};

/// IDAT payloads (zlib stream split at chunk boundaries) plus row geometry.
struct Png {
    name: String,
    chunks: Vec<Vec<u8>>,
    stride: usize,
    rows: usize,
    raw_len: usize,
}

fn parse_raw(path: &std::path::Path) -> Option<Png> {
    let data = std::fs::read(path).ok()?;
    let mut c = zenflate::Compressor::new(zenflate::CompressionLevel::libdeflate(6));
    let mut z = vec![0u8; zenflate::Compressor::zlib_compress_bound(data.len())];
    let n = c.zlib_compress(&data, &mut z, Unstoppable).ok()?;
    z.truncate(n);
    Some(Png {
        name: path.file_name()?.to_string_lossy().into_owned(),
        chunks: z.chunks(32 * 1024).map(<[u8]>::to_vec).collect(),
        stride: 4096,
        rows: data.len().div_ceil(4096),
        raw_len: data.len(),
    })
}

fn parse_png(path: &std::path::Path) -> Option<Png> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut pos = 8;
    let (mut width, mut height, mut bits, mut color, mut interlace) = (0u32, 0u32, 0u8, 0u8, 0u8);
    let mut chunks = Vec::new();
    while pos + 12 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let ty = &data[pos + 4..pos + 8];
        let body = data.get(pos + 8..pos + 8 + len)?;
        match ty {
            b"IHDR" => {
                width = u32::from_be_bytes(body[0..4].try_into().unwrap());
                height = u32::from_be_bytes(body[4..8].try_into().unwrap());
                bits = body[8];
                color = body[9];
                interlace = body[12];
            }
            b"IDAT" => chunks.push(body.to_vec()),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    let channels = match color {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let stride = 1 + (width as usize * channels * bits as usize).div_ceil(8);
    let joined: Vec<u8> = chunks.concat();
    let raw_len = miniz_oxide::inflate::decompress_to_vec_zlib(&joined)
        .ok()?
        .len();
    // Adam7 rows have varying strides; use the whole stream as "rows" of the
    // full-width stride for the streaming arm's consumption pattern.
    let rows = if interlace == 0 {
        height as usize
    } else {
        raw_len.div_ceil(stride)
    };
    Some(Png {
        name: path.file_stem()?.to_string_lossy().into_owned(),
        chunks,
        stride,
        rows,
        raw_len,
    })
}

/// IDAT chunks as an `InputSource`, one chunk per `fill_buf`.
struct Chunks<'a> {
    chunks: &'a [Vec<u8>],
    idx: usize,
    off: usize,
}

impl InputSource for Chunks<'_> {
    type Error = core::convert::Infallible;
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        while self.idx < self.chunks.len() && self.off >= self.chunks[self.idx].len() {
            self.idx += 1;
            self.off = 0;
        }
        Ok(match self.chunks.get(self.idx) {
            Some(c) => &c[self.off..],
            None => &[],
        })
    }
    fn consume(&mut self, n: usize) {
        self.off += n;
    }
}

#[inline(never)] // separate symbol for callgrind --toggle-collect
fn zen_oneshot(_p: &Png, joined: &[u8], out: &mut [u8]) -> usize {
    let mut d = Decompressor::new();
    d.zlib_decompress(joined, out, Unstoppable)
        .unwrap()
        .output_written
}

#[inline(never)] // separate symbol for callgrind --toggle-collect
fn zen_stream(p: &Png) -> usize {
    let whole = p.stride * p.rows;
    let capacity = (2 * p.stride).max((256 * 1024).min(whole));
    let src = Chunks {
        chunks: &p.chunks,
        idx: 0,
        off: 0,
    };
    let mut d = StreamDecompressor::zlib(src, capacity).with_ignore_checksum(true);
    let mut total = 0usize;
    let mut sink = 0u8;
    loop {
        while d.peek().len() < p.stride && !d.is_done() {
            d.fill().unwrap();
        }
        let avail = d.peek().len();
        if avail == 0 {
            break;
        }
        let take = avail.min(p.stride);
        sink ^= d.peek()[take - 1];
        d.advance(take);
        total += take;
    }
    std::hint::black_box(sink);
    total
}

#[inline(never)] // separate symbol for callgrind --toggle-collect
fn fdeflate_oneshot(joined: &[u8], out: &mut [u8]) -> usize {
    let mut d = fdeflate::Decompressor::new();
    d.ignore_adler32();
    let (_, n) = d.read(joined, out, 0, true).unwrap();
    n
}

#[inline(never)] // separate symbol for callgrind --toggle-collect
fn libdeflate_oneshot(joined: &[u8], out: &mut [u8]) -> usize {
    let mut d = libdeflater::Decompressor::new();
    d.zlib_decompress(joined, out).unwrap()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut rounds = 15usize;
    let mut filter = String::new();
    let mut profile: Option<String> = None;
    let mut raw = false;
    let mut secs = 5.0f64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rounds" => rounds = args.next().unwrap().parse().unwrap(),
            "--filter" => filter = args.next().unwrap(),
            "--profile" => profile = args.next(),
            "--raw" => raw = true,
            "--secs" => secs = args.next().unwrap().parse().unwrap(),
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
                .filter(|p| raw || p.extension().is_some_and(|e| e == "png"))
                .filter(|p| p.is_file())
                .collect();
            v.sort();
            files.extend(v);
        } else {
            files.push(i.clone());
        }
    }
    files.retain(|f| f.to_string_lossy().contains(&filter));

    if let Some(arm) = profile {
        let pngs: Vec<(Png, Vec<u8>)> = files
            .iter()
            .filter_map(|f| if raw { parse_raw(f) } else { parse_png(f) })
            .map(|p| {
                let j = p.chunks.concat();
                (p, j)
            })
            .collect();
        let max = pngs.iter().map(|p| p.0.raw_len).max().unwrap_or(0);
        let mut out = vec![0u8; max];
        let start = Instant::now();
        let mut n = 0u64;
        // At least one pass, so `--secs 0` gives exactly one (callgrind).
        while n == 0 || start.elapsed().as_secs_f64() < secs {
            for (p, j) in &pngs {
                let o = &mut out[..p.raw_len];
                std::hint::black_box(match arm.as_str() {
                    "zen1" => zen_oneshot(p, j, o),
                    "zenS" => zen_stream(p),
                    "fdef" => fdeflate_oneshot(j, o),
                    "libd" => libdeflate_oneshot(j, o),
                    _ => panic!("unknown arm {arm}"),
                });
                n += 1;
            }
        }
        println!("{arm}: {n} decodes");
        return;
    }
    println!(
        "{:<26} {:>9} {:>6} {:>9} {:>9} {:>9} {:>9}  {:>6} {:>6} {:>6}",
        "image",
        "raw KB",
        "chunks",
        "zen1 us",
        "zenS us",
        "fdef us",
        "libd us",
        "z1/fd",
        "zS/fd",
        "ld/fd"
    );
    let mut ratios = (Vec::new(), Vec::new(), Vec::new());
    for f in &files {
        let Some(p) = (if raw { parse_raw(f) } else { parse_png(f) }) else {
            continue;
        };
        let joined: Vec<u8> = p.chunks.concat();
        let mut out = vec![0u8; p.raw_len];
        // Correctness: every arm produces the full stream.
        assert_eq!(zen_oneshot(&p, &joined, &mut out), p.raw_len, "{}", p.name);
        let reference = out.clone();
        assert_eq!(zen_stream(&p), p.raw_len, "{}", p.name);
        out.fill(0);
        assert_eq!(fdeflate_oneshot(&joined, &mut out), p.raw_len, "{}", p.name);
        assert!(out == reference, "{}: fdeflate output differs", p.name);
        assert_eq!(
            libdeflate_oneshot(&joined, &mut out),
            p.raw_len,
            "{}",
            p.name
        );

        let mut t = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        // Repeat tiny inputs so one sample is at least ~2 ms.
        let reps = (2_000_000 / p.raw_len.max(1)).clamp(1, 2000);
        for _ in 0..rounds {
            let s = Instant::now();
            for _ in 0..reps {
                std::hint::black_box(zen_oneshot(&p, &joined, &mut out));
            }
            t[0].push(s.elapsed().as_secs_f64() * 1e6 / reps as f64);
            let s = Instant::now();
            for _ in 0..reps {
                std::hint::black_box(zen_stream(&p));
            }
            t[1].push(s.elapsed().as_secs_f64() * 1e6 / reps as f64);
            let s = Instant::now();
            for _ in 0..reps {
                std::hint::black_box(fdeflate_oneshot(&joined, &mut out));
            }
            t[2].push(s.elapsed().as_secs_f64() * 1e6 / reps as f64);
            let s = Instant::now();
            for _ in 0..reps {
                std::hint::black_box(libdeflate_oneshot(&joined, &mut out));
            }
            t[3].push(s.elapsed().as_secs_f64() * 1e6 / reps as f64);
        }
        let m: Vec<f64> = t.iter_mut().map(|v| median(v)).collect();
        println!(
            "{:<26} {:>9.1} {:>6} {:>9.1} {:>9.1} {:>9.1} {:>9.1}  {:>6.2} {:>6.2} {:>6.2}",
            p.name,
            p.raw_len as f64 / 1024.0,
            p.chunks.len(),
            m[0],
            m[1],
            m[2],
            m[3],
            m[0] / m[2],
            m[1] / m[2],
            m[3] / m[2]
        );
        ratios.0.push(m[0] / m[2]);
        ratios.1.push(m[1] / m[2]);
        ratios.2.push(m[3] / m[2]);
    }
    let n = ratios.0.len();
    println!(
        "median ratio vs fdeflate over {n} images: zen one-shot {:.3}, zen stream {:.3}, libdeflate {:.3}",
        median(&mut ratios.0),
        median(&mut ratios.1),
        median(&mut ratios.2)
    );
}

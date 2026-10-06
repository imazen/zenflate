//! Worst gap between `Stop::check()` calls, per level: how long a cancel or
//! deadline can go unnoticed. The stop never fires; it only records the time
//! between polls (and before the first / after the last one).
//!
//! ```text
//! cargo run --release --example stop_latency [-- MiB [level,...]]
//! ```
//! Level specs: `e<N>` = `CompressionLevel::new(N)`, `l<N>` = `libdeflate(N)`.
use std::sync::Mutex;
use std::time::Instant;

use zenflate::{CompressionLevel, Compressor};

struct Meter {
    last: Mutex<(Instant, f64, u64)>,
}

impl enough::Stop for Meter {
    fn check(&self) -> Result<(), enough::StopReason> {
        let mut g = self.last.lock().unwrap();
        let now = Instant::now();
        let gap = now.duration_since(g.0).as_secs_f64();
        g.0 = now;
        g.1 = g.1.max(gap);
        g.2 += 1;
        Ok(())
    }
}

fn level(spec: &str) -> CompressionLevel {
    let n: u32 = spec[1..].parse().expect("level number");
    match &spec[..1] {
        "e" => CompressionLevel::new(n),
        "l" => CompressionLevel::libdeflate(n),
        _ => panic!("level spec is e<N> or l<N>"),
    }
}

/// Image-like rows with runs, repeats and noise, plus a text-like tail.
fn input(len: usize) -> Vec<u8> {
    let mut s = 0x9e37_79b9u32;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        match (out.len() / 4096) % 4 {
            0 => out.push(0),
            1 => out.push((s % 7) as u8),
            2 => {
                let b = out[out.len() - 3000];
                out.push(b);
            }
            _ => out.push(s as u8),
        }
    }
    out
}

fn main() {
    let mib: usize = std::env::args().nth(1).map_or(4, |s| s.parse().unwrap());
    let specs: Vec<String> = std::env::args().nth(2).map_or_else(
        || {
            [
                "e1", "e5", "e10", "e15", "e20", "e24", "e30", "e31", "l1", "l6", "l12",
            ]
            .map(String::from)
            .to_vec()
        },
        |s| s.split(',').map(String::from).collect(),
    );
    let data = input(mib << 20);
    println!(
        "{} MiB input; worst gap between polls (ms), polls, total ms",
        mib
    );
    for spec in specs {
        let meter = Meter {
            last: Mutex::new((Instant::now(), 0.0, 0)),
        };
        let mut c = Compressor::new(level(&spec));
        let mut out = vec![0u8; Compressor::zlib_compress_bound(data.len())];
        let t0 = Instant::now();
        *meter.last.lock().unwrap() = (t0, 0.0, 0);
        c.zlib_compress(&data, &mut out, &meter).unwrap();
        let total = t0.elapsed().as_secs_f64();
        let (last, worst, polls) = *meter.last.lock().unwrap();
        let tail = last.elapsed().as_secs_f64();
        println!(
            "{spec:5} worst {:8.2}  polls {:7}  total {:9.1}",
            worst.max(tail) * 1e3,
            polls,
            total * 1e3
        );
    }
}

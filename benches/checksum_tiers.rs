//! Per-kernel NEON-vs-forced-scalar for the checksum kernels.
//!
//! adler32 and crc32 run over every compressed byte, so they are on the
//! critical path of every encode and decode. `throughput.rs` measures whole
//! compression, which cannot reveal a checksum kernel being SLOWER than its
//! own scalar fallback — that failure mode was real in this sweep (three
//! zenfilters NEON kernels lost to their scalar tier).
//!
//! NOTE: on aarch64 NEON is BASELINE, so the "scalar" arm is the magetypes
//! scalar tier WITH autovectorization. ~1.00x means both compiled to
//! equivalent work; BELOW 1.00 is the bug this exists to catch.
//!
//! crc32 is additionally interesting on aarch64 because the ISA has dedicated
//! CRC instructions — if the kernel is not using them, the scalar arm can
//! genuinely win.
//!
//! Run: `cargo bench --bench checksum_tiers`

use zenbench::prelude::*;
use zenflate::checksum::{adler32, crc32};

#[cfg(target_arch = "aarch64")]
type TierToken = archmage::NeonToken;
#[cfg(target_arch = "x86_64")]
type TierToken = archmage::X64V3Token;

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const TIER_NAME: &str = if cfg!(target_arch = "aarch64") { "neon" } else { "v3(avx2)" };

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn set_simd(enabled: bool) -> bool {
    TierToken::dangerously_disable_token_process_wide(!enabled).is_ok()
}
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn set_simd(_e: bool) -> bool { false }

fn data(n: usize) -> Vec<u8> {
    let mut s = 0x9e37_79b9u32;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

fn bench_checksums(suite: &mut Suite) {
    if !set_simd(true) || !set_simd(false) {
        eprintln!("[checksum_tiers] SIMD tier not toggleable here. Skipping.");
        return;
    }
    set_simd(true);
    eprintln!("[checksum_tiers] comparing {TIER_NAME} vs forced scalar");

    // 64 KiB is a realistic deflate window-ish chunk; 4 MiB shows steady state
    // once per-call overhead is amortized.
    for &(label, n) in &[("64KiB", 64 * 1024usize), ("4MiB", 4 * 1024 * 1024)] {
        let buf: &'static [u8] = Box::leak(data(n).into_boxed_slice());
        for (name, is_crc) in [("adler32", false), ("crc32", true)] {
            suite.compare(format!("{name}/{label}"), |g| {
                g.throughput(Throughput::Bytes(n as u64));
                for (arm, simd) in [(TIER_NAME, true), ("scalar", false)] {
                    g.bench(arm, move |b| {
                        b.iter(move || {
                            set_simd(simd);
                            if is_crc { crc32(0, buf) } else { adler32(1, buf) }
                        })
                    });
                }
            });
        }
    }
    set_simd(true);
}

zenbench::main!(bench_checksums);

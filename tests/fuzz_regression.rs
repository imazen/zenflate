//! Fuzz crash regression suite (DEDUP-J template, ported from zenwebp).
//!
//! Runs every file in `fuzz/regression/` through the raw-bytes decompression
//! entry points covered by the `fuzz_decompress` fuzz target. Each seed file
//! is a previously-found crash that has been fixed; this test ensures none of
//! them re-introduce a panic.
//!
//! The `fuzz_roundtrip` target takes arbitrary-encoded `Input { effort, data }`
//! tuples instead of raw bytes; regression seeds for that target are not
//! handled by this harness today (they would require pulling in the
//! `arbitrary` dev-dep to deserialize). When the first roundtrip regression
//! seed appears, extend this harness to run them through
//! `arbitrary::Unstructured::new(&bytes)` + `Input::arbitrary(&mut u)`.
//!
//! To add a new decompress seed: drop the (preferably minimized) crash file
//! into `fuzz/regression/` (or a per-target subdir under it), no other action
//! required.

use std::fs;
use std::path::PathBuf;

use zenflate::{Decompressor, Unstoppable};

fn regression_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/regression")
}

/// Recursively collect every regular file under `dir`. Skips dotfiles and
/// README-style meta files, and silently tolerates a missing directory.
fn collect_seeds(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let read = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.eq_ignore_ascii_case("README.md") {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_file() => out.push(path),
            Ok(t) if t.is_dir() => collect_seeds(&path, out),
            _ => {}
        }
    }
}

fn run_decompress(input: &[u8]) {
    // Mirrors fuzz_targets/fuzz_decompress.rs.
    let mut d = Decompressor::new();
    let mut output = vec![0u8; 64 * 1024];

    let _ = d.deflate_decompress(input, &mut output, Unstoppable);
    let _ = d.zlib_decompress(input, &mut output, Unstoppable);
    let _ = d.gzip_decompress(input, &mut output, Unstoppable);
}

#[test]
fn fuzz_regression_seeds_do_not_panic() {
    let dir = regression_dir();
    let mut seeds = Vec::new();
    collect_seeds(&dir, &mut seeds);

    if seeds.is_empty() {
        eprintln!(
            "note: no regression seeds found under {} — nothing to check",
            dir.display()
        );
        return;
    }

    for path in seeds {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unnamed>")
            .to_owned();
        let input = fs::read(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));

        // Each entry point may return Err but must not panic. If any panics,
        // the test fails with the seed name in the unwind message.
        run_decompress(&input);

        eprintln!("ok: {name} ({} bytes)", input.len());
    }
}

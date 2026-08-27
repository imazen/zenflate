//! Replay seed inputs from `fuzz/regression/` through every fuzz target
//! entry point. Shared scaffolding lives in `zen-fuzz-regress`.

use std::path::Path;
use zenflate::{Decompressor, Unstoppable};
use zenutils_fuzz::RegressionSuite;

/// Lower bound on the replayable seed corpus committed under `fuzz/regression/`.
///
/// **This is deliberately zero, and zenflate is the only zen codec where it is.**
/// The corpus holds no minimized crash inputs yet: #7 (a 2^23+ literal run
/// overflowing `litrunlen` into the length field of `Sequence`) reproduces only
/// from a ~19 MB input — a 10.3 MB 'A' match prefix plus `(1 << 23) + 4096` PRNG
/// literals — which is four orders of magnitude past the 8 KB per-seed ceiling in
/// CLAUDE.md, so it is gated by the unit test in `src/compress/full_optimal.rs`
/// instead of a committed seed (commit 486e04ce). The zero is pinned
/// here so the empty corpus is a visible decision rather than an accident —
/// `RegressionSuite` silently no-ops on an empty directory, so nothing else in
/// the chain would tell you the suite replayed nothing.
///
/// Raise this to the new count the moment a seed lands.
const MIN_SEEDS: usize = 0;

/// Count the files `RegressionSuite::run` will actually replay, using its own
/// filters: recurse into subdirectories, skip dotfiles, `*.md` and `*.txt`.
fn replayable_seeds(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut found = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            found += replayable_seeds(&path);
        } else if path.is_file() {
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".md") && !lower.ends_with(".txt") {
                found += 1;
            }
        }
    }
    found
}

/// Fail loudly when the corpus this suite exists to replay is not there.
///
/// With `MIN_SEEDS == 0` the count assertion is currently slack, so the
/// directory check is what carries the weight: a renamed or unchecked-out
/// `fuzz/regression/` fails here instead of passing silently.
fn assert_corpus_present() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/regression");
    assert!(
        dir.is_dir(),
        "{} is missing — the regression corpus directory is tracked in git \
         (it holds README.md documenting how to add seeds); without it this \
         suite would replay nothing and still report success",
        dir.display()
    );
    let found = replayable_seeds(&dir);
    assert!(
        found >= MIN_SEEDS,
        "{} holds {found} replayable seeds, expected at least {MIN_SEEDS} — \
         seeds were deleted without lowering MIN_SEEDS",
        dir.display()
    );
}

#[test]
fn fuzz_regression() {
    assert_corpus_present();
    RegressionSuite::new("fuzz/regression")
        .target("decompress", |data| {
            let mut d = Decompressor::new();
            let mut output = vec![0u8; 64 * 1024];
            let _ = d.deflate_decompress(data, &mut output, Unstoppable);
            let _ = d.zlib_decompress(data, &mut output, Unstoppable);
            let _ = d.gzip_decompress(data, &mut output, Unstoppable);
        })
        .run();
}

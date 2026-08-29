//! Replay seed inputs from `fuzz/regression/` through every fuzz target
//! entry point. Shared scaffolding lives in `zen-fuzz-regress`.

use std::path::Path;
use zenflate::{Decompressor, Unstoppable};
use zenutils_fuzz::RegressionSuite;

/// Exact number of replayable seeds committed under `fuzz/regression/`.
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
/// Checked with `assert_eq!`, **not** a `>=` floor. A floor of zero is satisfied
/// by every `usize` that can exist, so it gates nothing — the first version of
/// this harness shipped exactly that, and clippy's `absurd_extreme_comparisons`
/// rejected it: an always-true assertion inside the guard written to stop gates
/// that cannot fail. Equality gates in both directions instead — a deleted seed
/// fails, and a seed that lands without this constant being raised fails too,
/// which is what forces the corpus to stay a reviewed decision.
///
/// Raise this to the new count the moment a seed lands.
const EXPECTED_SEEDS: usize = 0;

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
/// Three distinct ways the replay can silently become a no-op, each checked:
/// the directory is gone, the directory is there but empty, or the seed count
/// has drifted from what the repo committed.
fn assert_corpus_present() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/regression");
    assert!(
        dir.is_dir(),
        "{} is missing — the regression corpus directory is tracked in git \
         (it holds README.md documenting how to add seeds); without it this \
         suite would replay nothing and still report success",
        dir.display()
    );

    // README.md is the only tracked file in the corpus directory, and therefore
    // the only reason git materialises the directory at all — git does not track
    // empty directories. Check it explicitly: with the corpus legitimately empty,
    // deleting the README is the one mutation that empties `fuzz/regression/`
    // without tripping either the `is_dir` check above (the directory survives in
    // a dirty worktree) or the count check below (0 == 0 still holds).
    let readme = dir.join("README.md");
    assert!(
        readme.is_file(),
        "{} is missing — it is the only tracked file in the regression corpus, \
         so without it git stops materialising {} entirely and every seed added \
         beside it goes with the directory",
        readme.display(),
        dir.display()
    );

    let found = replayable_seeds(&dir);
    assert_eq!(
        found,
        EXPECTED_SEEDS,
        "{} holds {found} replayable seeds, but this harness is pinned to \
         {EXPECTED_SEEDS}. If seeds were added, raise EXPECTED_SEEDS to {found} \
         so the corpus size stays a reviewed decision. If seeds were deleted, \
         restore them — the suite replays exactly what is in this directory, so \
         a missing seed is a regression gate that silently stopped running.",
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

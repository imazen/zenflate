//! Replay seed inputs from `fuzz/regression/` through every fuzz target
//! entry point. Shared scaffolding lives in `zen-fuzz-regress`.

use std::path::Path;
use zenflate::{Decompressor, Unstoppable};

/// The `fuzz_api` target's checks, replayed here on stable.
mod api {
    #![allow(dead_code)]
    include!("../fuzz/fuzz_targets/api_check.rs");
}
/// The `fuzz_inflate_diff` target's check, replayed here on stable.
mod inflate_diff {
    #![allow(dead_code)]
    include!("../fuzz/fuzz_targets/inflate_diff_check.rs");
}
use zenutils_fuzz::RegressionSuite;

/// Exact number of replayable seeds committed under `fuzz/regression/`.
///
/// Checked with `assert_eq!`, not a `>=` floor: a deleted seed fails, and a
/// seed that lands without this constant being raised fails too, so the
/// corpus stays a reviewed decision. (`RegressionSuite` silently no-ops on an
/// empty directory.) Raise this the moment a seed lands.
///
/// `fuzz_api/stop-then-reuse-effort{1,15,24}.bin`: a compressor stopped by its
/// `Stop` token, then reused (panicked before 1383ac5).
/// `fuzz_api/png3-stop-then-reuse.bin`: the same for the `png()` hash parser.
/// `fuzz_api/png1-bound-crossover.bin`: a 266-byte input whose `png(1)` block
/// landed 1-2 bytes past `zlib_compress_bound` (stored fallback compared
/// bytes, not bits). `fuzz_api/png1-short-buffer-cap{8,12}.bin`: `png(1)`
/// into an 8- or 12-byte buffer pushed the bit buffer past 64 bits (debug
/// assertion / shift overflow) before 4a17814. Bugs whose
/// reproducers exceed the 8 KB seed ceiling are gated by unit tests instead:
/// #7's 19 MB literal run (`full_optimal.rs`), incremental calls past one
/// sequence store and parallel full-optimal (`compress/mod.rs`).
const EXPECTED_SEEDS: usize = 7;

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
        .target("fuzz_inflate_diff", inflate_diff::check)
        .target("fuzz_api", |data| {
            use arbitrary::Arbitrary;
            if let Ok(input) = api::Input::arbitrary_take_rest(arbitrary::Unstructured::new(data)) {
                api::check(&input);
            }
        })
        .run();
}

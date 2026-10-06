# Fuzz regression seeds

This directory holds previously-found crash inputs that have been fixed.
The `cargo test -p zenflate --test fuzz_regression` harness walks this
directory (recursively, ignoring dotfiles and README.md) and runs each
file through the `fuzz_decompress` entry points (`deflate`, `zlib`,
`gzip` decompression with a 64 KB output buffer).

Seeds are also replayed through the `fuzz_api` checks
(`fuzz/fuzz_targets/api_check.rs`, shared by the fuzz target and the test):
the bytes decode as its arbitrary `Input` (level, entry point, stop point,
buffer size, threads, data). Keep those under `fuzz_api/`. A seed only needs
to reproduce a fixed bug on the code before the fix; check that before
committing it, and raise `EXPECTED_SEEDS` in the harness.

To add a seed:
1. Minimize the crash with `cargo +nightly fuzz tmin <target> <input>`.
2. Verify it's small (target ≤ 1 KB, hard ceiling 8 KB per CLAUDE.md).
3. Drop it into this directory (optionally under a `fuzz_<target>/`
   subdir for organization) with a descriptive name.
4. Re-run the regression harness to confirm it passes on the fix.

Per CLAUDE.md "Fuzz Corpus & Crash Storage": the working fuzz corpus
and unminimized crashes live in `/mnt/v/fuzzes/zenflate/`, NOT in git.

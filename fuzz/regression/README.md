# Fuzz regression seeds

This directory holds previously-found crash inputs that have been fixed.
The `cargo test -p zenflate --test fuzz_regression` harness walks this
directory (recursively, ignoring dotfiles and README.md) and runs each
file through the `fuzz_decompress` entry points (`deflate`, `zlib`,
`gzip` decompression with a 64 KB output buffer).

The `fuzz_roundtrip` target uses an arbitrary-encoded `Input { effort,
data }` struct rather than raw bytes; if you add regression seeds for
it, also extend `tests/fuzz_regression.rs` to deserialize them via
`arbitrary::Unstructured`.

To add a seed:
1. Minimize the crash with `cargo +nightly fuzz tmin <target> <input>`.
2. Verify it's small (target ≤ 1 KB, hard ceiling 8 KB per CLAUDE.md).
3. Drop it into this directory (optionally under a `fuzz_<target>/`
   subdir for organization) with a descriptive name.
4. Re-run the regression harness to confirm it passes on the fix.

Per CLAUDE.md "Fuzz Corpus & Crash Storage": the working fuzz corpus
and unminimized crashes live in `/mnt/v/fuzzes/zenflate/`, NOT in git.

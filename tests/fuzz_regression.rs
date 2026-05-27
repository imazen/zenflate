//! Replay seed inputs from `fuzz/regression/` through every fuzz target
//! entry point. Shared scaffolding lives in `zen-fuzz-regress`.

use zenutils_fuzz::RegressionSuite;
use zenflate::{Decompressor, Unstoppable};

#[test]
fn fuzz_regression() {
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

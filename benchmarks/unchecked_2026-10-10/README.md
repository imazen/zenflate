# Safe versus unchecked, 2026-10-10

Commit: `8c74e3f1966890cbf3d2ce2451a5e5fee8a609da`.
25 PNG variants from four source images; sizes 64, 256, 1024 and 2560,
with gray8, RGB16 and RGBA8 variants at 1024. Three process runs per build;
each encode case uses three timing rounds, each decode case nine.
Safe/unchecked order alternates. Each table time is the median of the three
per-run sums over images. U/S is their ratio; the range uses paired run ratios.
Values below 1 mean unchecked took less time. Encoded sizes match in every case.

| Operation | Safe ms | Unchecked ms | U/S | Paired range |
|---|---:|---:|---:|---:|
| zen_e1 | 103.698 | 100.914 | 0.9731 | 0.9710–0.9807 |
| zen_e10 | 236.940 | 220.609 | 0.9311 | 0.9133–0.9486 |
| zen_e15 | 550.181 | 536.744 | 0.9756 | 0.9313–0.9762 |
| zen_e24 | 5451.885 | 5798.043 | 1.0635 | 1.0635–1.0667 |
| zen_e30 | 14648.283 | 16308.007 | 1.1133 | 1.1106–1.1139 |
| zen_ld1 | 149.754 | 138.163 | 0.9226 | 0.9218–0.9265 |
| zen_ld12 | 11973.438 | 13277.228 | 1.1089 | 1.1072–1.1099 |
| zen_ld6 | 425.325 | 407.493 | 0.9581 | 0.9580–0.9619 |
| zen_png1 | 24.100 | 23.611 | 0.9797 | 0.9580–1.0080 |
| zen_png10 | 395.342 | 372.409 | 0.9420 | 0.9089–0.9439 |
| zen_png19 | 4064.040 | 4382.633 | 1.0784 | 1.0764–1.0812 |
| zen_png30 | 14863.712 | 16423.622 | 1.1049 | 1.1032–1.1071 |
| zen_png4 | 132.391 | 127.362 | 0.9620 | 0.9606–0.9629 |
| one-shot decode | 30.713 | 31.974 | 1.0411 | 1.0325–1.0441 |
| streaming decode | 29.504 | 29.466 | 0.9987 | 0.9982–0.9992 |

## Scope and reproduction

Measured on an Intel Core Ultra 7 265K, pinned to CPU 2, using Rust 1.98.1
and LLVM 22.1.8. Both RUSTFLAGS variables were empty; no target-cpu=native.
Default features versus default features plus unchecked, with archmage 0.9.30.
The existing example harnesses enable archmage/testable_dispatch through a
dev-dependency in both arms. This is a feature comparison on this host and
workload, not a cross-platform or ecosystem ranking.

Inputs were the existing PNG variants in the vs_png benchmark cache. Filenames
and SHA-256 hashes are in inputs.sha256; all hashes were checked again after
measurement and matched. No inputs or binaries are included in this record.
The same filtered bytes feed every compression setting. The timing includes
compression into the harness's reused compressor/output buffers. Each decode
arm constructs its decoder; streaming consumes rows. The scripts and original
harnesses at the recorded commit define the exact timing boundaries.

```sh
TMPDIR="$HOME/tmp" run-heavy --mem 12G --jobs 8 -- \
  just compare-unchecked <png-input-directory> <new-results-directory>
just summarize-unchecked <results-directory>
```

The driver acquires the shared benchmark locks and alternates process order:
safe/unchecked, unchecked/safe, safe/unchecked. Raw encode TSVs and decode
output, compiler/CPU metadata, and binary hashes are retained here. Recompute
the table with `python3 scripts/summarize-unchecked.py benchmarks/unchecked_2026-10-10`.
The three paired ratios show run variation; they are not confidence intervals.
The 25 variants derive from four source images, so they are not 25 independent
source images. No ARM or AVX-512 host was measured in this experiment.

Resource record for builds and all six measurement runs:

```text
run-heavy: done rc=0 1126s | peak-RSS 0.30GiB | min-avail 27895MiB | peak-load 1.47
```

## Interpretation

The safe implementation already uses fixed-size matchfinder tables with masked
indices, a 259-node DP view, and a 65536-entry offset-slot view. Both feature
configurations use the same large-buffer storage types. The unchecked variant
is useful for some lighter compression settings here, but the safe variant takes
less time on every tested near-optimal setting. This experiment does not isolate
which raw-pointer loop causes that difference. One-shot decode also takes more
time in the unchecked binary; streaming differs by less than 0.2% in the table.
Keep the safe default; measure a consumer's actual workload before opting in.

Archmage 0.9.30 fixes a dispatch-disable race and macro handling. Its tokenless
magetypes rite support and magetypes gather/scatter additions are not used by
zenflate's current hot loops; this run does not compare archmage versions.
See the [published changelog](https://docs.rs/crate/archmage/0.9.30/source/CHANGELOG.md).

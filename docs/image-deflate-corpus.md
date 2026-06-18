# An image DEFLATE corpus (not a text one)

Canterbury and Silesia are text/binary corpora. They're the wrong yardstick for the
DEFLATE that runs inside image codecs, and they miss the regime where image DEFLATE
actually spends its time. This note explains what an image-DEFLATE corpus should be,
points at the source we have, and reports what zenflate vs zlib-rs looks like on it.

Numbers come from `benchmarks/image_deflate_corpus_2026-06-18.txt`, produced by
`examples/image_deflate_corpus.rs` (zenflate 0.3.6 `--features unchecked`, zlib-rs
0.6, Ryzen 9 7950X, WSL2). Reproduce with
`IMG_PER_CLASS=4 cargo run --release --features unchecked --example image_deflate_corpus`.

## Image DEFLATE compresses residuals, not pixels

PNG and TIFF don't hand raw pixels to DEFLATE. They decorrelate first:

- **PNG** picks a filter per row (None/Sub/Up/Average/Paeth) and DEFLATEs the
  filtered residual stream. The adaptive filter is part of the format.
- **TIFF "Deflate"/"Adobe Deflate"** usually pairs with Predictor 2 (horizontal byte
  differencing) for continuous-tone images; Predictor 3 for floating-point.

The transform changes the byte statistics DEFLATE sees more than the choice of
compressor does. On our set, going raw → PNG-filtered moves the aggregate ratio from
2.76x to 3.62x (+31%) at level 6, while swapping zenflate for zlib-rs at a fixed
transform moves it ~5%. A corpus that compresses raw pixels (or text) is measuring a
workload no image codec actually runs.

So the corpus is a *transform pipeline over images*, not a pile of files:

```
raw         row-major pixels, no predictor     (TIFF Deflate, predictor 1)
tiff_pred2  horizontal byte differencing       (TIFF Adobe Deflate, common case)
png_filter  per-row adaptive PNG filter         (what PNG encoders feed DEFLATE)
```

One caveat worth stating: **GIF is LZW, not DEFLATE** — that's the `zenlzw` crate, not
zenflate. Palette-index streams (GIF, PNG-palette) are a real and distinct data class,
but they belong to a separate benchmark. This corpus covers the two DEFLATE image
formats, PNG and TIFF.

## Source: the imazen-26 PNG set

`/mnt/v/output/imazen-26-png` is content-classed and public-domain/licensed, which is
exactly what the sweep discipline wants — content variation across photo, screen,
line-art, document, and mixed. The numeric-prefix taxonomy maps cleanly to
DEFLATE-relevant classes:

| class | imazen-26 source | what DEFLATE sees |
|---|---|---|
| photo | lilith/unsplash/museum photos (1000–3300) | high-entropy continuous tone |
| texture / render | unsplash textures (2400), renders (2200) | semi-structured |
| document | NPS/EPA/NOAA docs (5000–5300) | text-on-paper, bimodal |
| scan | patents, manuscripts (6000–6800) | paper texture + ink |
| plot | charts (7000) | flat fields + thin lines |
| screenshot | mobile/web UI (8000–8100) | flat regions, repeated UI |
| clipart / icon | AI clipart (9000+), icon sheets | synthetic flat color |

Standard conformance sets are also cached and useful as portable, reproducible inputs
(`~/.cache/codec-corpus/v1/`): `pngsuite`, `png-conformance`, `CID22`, `clic2025`, and
`tiff-conformance` (which includes a real Deflate TIFF,
`valid/deflate-last-strip-extra-data.tiff`). imazen-26 is the better *content* corpus;
the conformance sets are the better *portability* fallback.

## What the corpus shows

Two things text corpora hide.

**Content class spans ~20x, and the high-ratio classes are the point.** At level 6,
PNG-filtered: photo 2.46x, texture 1.96x, scan 2.41x sit near the Canterbury/Silesia
range — but document 12.1x, plot 11.1x, screenshot 10.6x, and icon/clipart 35x+ are a
different world. Web image-DEFLATE is dominated by screenshots, UI, documents, and
synthetic graphics, none of which text corpora represent. If your benchmark is only
photos, you're testing the *least* compressible, least filter-sensitive end.

**The filter helps the structured classes most.** raw → png_filter barely moves photos
(1.69x → 2.46x is mostly the predictor doing edge work) but reshapes screenshots and
plots. Predictor 2 and the adaptive PNG filter land close on photos; the adaptive
filter pulls ahead on mixed content where the best filter changes row to row.

## zenflate vs zlib-rs on image residuals

Same shape as the text result, and it holds across every class and transform
(`benchmarks/image_deflate_corpus_2026-06-18.txt`):

- **Ratio:** zlib-rs is ~5% ahead at matched level, and stays ~3% ahead even at
  zenflate's max effort (png_filter aggregate: zenflate L12 3.84x vs zlib-rs L9 3.95x).
  On these residual streams zenflate's near-optimal doesn't fully close the ratio gap
  the way it does on text.
- **Compress speed:** zenflate is 3–4x faster at L6 (png_filter aggregate 374 vs 112
  MiB/s) and ~6x faster at max effort (176 vs 30 MiB/s) — the gap widens with effort
  because zlib-rs L9 on filtered image data is very slow (down to single-digit MiB/s on
  scans and photos).
- **Decompress:** zlib-rs is ~1.3–1.5x faster (png_filter aggregate 1121 vs 1625 MiB/s).

The trade for an image pipeline: zenflate encodes filtered rows several times faster at
a few percent larger output, and decodes somewhat slower. For a PNG/TIFF *encoder*
(zenpng, zentiff) that's a strong win — encode speed at scale matters and the ratio gap
is small. For a read-heavy decode path it's the usual zlib-rs-decodes-faster story.

## Gaps and next steps

This is a v1 demonstrator. To make it a calibration-grade corpus (the bar in CLAUDE.md
for anything that informs `const`s or level maps):

- **Size sweep.** Each image is center-cropped to ≤1 MP to bound wall time; that fixes
  the size axis. A real sweep needs 16–20 log-spaced sizes per source (the
  `imazen26-sat-scan` set is already multi-size: ds1024/1536/2048).
- **16-bit.** Decoding is 8-bit only right now. Scientific/medical TIFFs are 16-bit and
  compress differently; add depth as an axis.
- **Palette / index streams.** The GIF/PNG-palette class (and `zenlzw`) deserves its own
  harness keyed on index-byte statistics.
- **Predictor 3 and per-filter breakdown.** Float predictor for HDR/scientific, and
  reporting which PNG filter won per class.
- **Portability.** The default path is local (`/mnt/v`). Wire the cached conformance
  sets (`pngsuite`, `CID22`) as a fallback via `codec-corpus` so it runs in CI.

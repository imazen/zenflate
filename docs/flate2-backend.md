# Can zenflate be a flate2 backend?

Short answer: yes, but not as a plug-in. flate2 picks its backend at compile time
from a fixed set of in-tree modules, so "being a backend" means landing a module
inside flate2 itself (upstream PR or fork), not implementing a trait from zenflate.
The trait surface is small; the real work is a streaming `Compress`/`Decompress`
state machine that zenflate doesn't have yet. zenflate's inflate is already a
resumable state machine, so decompression is most of the way there. Compression is
the long pole — zenflate today is a one-shot compressor.

This is an analysis, not a commitment to build it. Numbers cited come from the
committed benchmark runs in `benchmarks/zlib_rs_compare_2026-06-18.txt` and
`benchmarks/throughput_interleaved_2026-06-18.txt` (AMD Ryzen 9 7950X, WSL2,
zenflate 0.3.6 `--features unchecked`, zlib-rs 0.6, flate2 1.1 on its zlib-rs backend).

## How flate2 selects a backend

flate2 has one private module, `ffi`, that re-exports exactly one backend's
`Deflate` and `Inflate` types under fixed names. Which one is chosen by mutually
exclusive cargo features (`rust_backend`/miniz_oxide, `zlib`, `zlib-ng`,
`cloudflare_zlib`, `zlib-rs`), each gated on an optional dependency in flate2's own
`Cargo.toml`. `src/mem.rs` calls those types by name; the rest of the crate
(`Compress`, `Decompress`, `GzEncoder`, the `read`/`write`/`bufread` adaptors)
sits on top unchanged.

Two consequences:

- **`mod ffi` is private.** The backend traits (`Backend`, `DeflateBackend`,
  `InflateBackend`) are crate-internal. A downstream crate cannot implement them.
  There is no registration hook, no `dyn Backend`, no runtime selection.
- **A new backend is a new in-tree module plus feature plumbing.** That is how
  zlib-rs got added. To make `zenflate` a peer, you add a `zenflate` feature + an
  optional `zenflate` dep + `src/ffi/zenflate.rs` to flate2. That ships only via an
  upstream PR to rust-lang/flate2-rs or a maintained fork.

## The contract a backend must satisfy

From `src/ffi/mod.rs` and the zlib-rs backend (`src/ffi/zlib_rs.rs`), a backend
provides two types and implements three traits:

```rust
pub trait Backend: Sync + Send {
    fn total_in(&self) -> u64;
    fn total_out(&self) -> u64;
}
pub trait DeflateBackend: Backend {
    fn make(level: Compression, zlib_header: bool, window_bits: u8) -> Self;
    fn compress(&mut self, input: &[u8], output: &mut [u8], flush: FlushCompress)
        -> Result<Status, CompressError>;
    fn reset(&mut self);
}
pub trait InflateBackend: Backend {
    fn make(zlib_header: bool, window_bits: u8) -> Self;
    fn decompress(&mut self, input: &[u8], output: &mut [u8], flush: FlushDecompress)
        -> Result<Status, DecompressError>;
    fn reset(&mut self, zlib_header: bool);
}
```

Plus: the `MZ_*` flush constants, an `ErrorMessage` type, a `Status` mapping
(`Ok` / `BufError` / `StreamEnd`), `set_dictionary` on both sides, and `set_level`
on the deflate side. Roughly 250 lines for the zlib-rs backend, because zlib-rs
already exposes a matching streaming API — flate2's module is a thin shim.

Three things that look like requirements but aren't:

- **Sub-32K windows.** `make` takes `window_bits: u8` in 9..=15. The miniz_oxide
  backend ignores it (`_window_bits`) and always uses the full 32K window.
  zenflate's fixed 32K window is the same posture as the default pure-Rust backend,
  so this is not a gap.
- **gzip framing.** flate2's `gz` module does its own gzip header + CRC over a
  raw-deflate backend; the `new_gzip` path additionally asks the backend for gzip
  via the `window_bits + 16` convention. zenflate has gzip framing already, and the
  raw-deflate path is what's load-bearing regardless.
- **Byte-identical output.** flate2 makes no cross-backend output guarantee. A
  different bitstream is fine; only round-trip correctness is tested.

## Where zenflate stands today

### Decompression — most of the way there

`StreamDecompressor` (`src/decompress/streaming.rs`) is already a resumable inflate
state machine: an 8-state enum (`WrapperHeader` → `BlockHeader` →
`DynamicPrecodeLens`/`DynamicCodeLengths` → `CompressedData`/`UncompressedData` →
`WrapperFooter` → `Done`), pulling input through an `InputSource` and tracking
produced bytes for back-reference bounds. The hard algorithmic part — suspending and
resuming mid-block — exists.

What's missing for the flate2 shape:

- It writes into its **own** ring buffer (`peek`/`advance`), not a caller-owned
  `&mut [u8]`. The backend contract hands you the output slice. Needs an entry point
  that decodes directly into a provided slice and returns `BufError` when it fills.
- An input adaptor that consumes a `&[u8]` chunk and reports bytes consumed
  (`total_in`), leaving the remainder for the next call.
- `total_in`/`total_out` counters, the `Status`/`FlushDecompress` mapping, and the
  `NeedsDictionary(adler)` path + `inflateSetDictionary` equivalent.

This is a moderate refactor — parameterize the output sink, add counters and the
dictionary path. No new algorithm.

### Compression — the long pole

zenflate is a one-shot compressor. `deflate_compress(input, output)` produces the
whole stream in a single call. `deflate_compress_incremental` exists but is a
different tool: it requires the **full accumulated input** every call (a superset
with the same prefix), returns a hard `InsufficientSpace` error on output overflow
instead of resuming, and supports only the greedy/lazy strategies (not Turbo/FastHt/
NearOptimal, and not levels 0 or 10–12). It was built for the PNG filter-forking use
case, not general streaming.

The backend contract needs the opposite: arbitrary input chunks in, a caller-owned
output slice that can fill at any point, and a resume that picks up mid-block on the
next call — across all five flush modes (`None`, `Partial`, `Sync`, `Full`,
`Finish`) and all levels. `Full` also resets the history window. zenflate's bit
emitter (`OutputBitstream`) writes one block at a time into one buffer and errors on
overflow; it cannot pause mid-symbol and resume into a fresh buffer.

You don't have to make the encoder itself suspendable. The tractable design is a
streaming layer wrapped around the existing one-shot block compressor:

1. Accumulate input internally until you have a block's worth, or a flush/`Finish`
   forces it.
2. Compress that block one-shot into an internal scratch buffer (reusing today's
   encoder).
3. Meter the scratch buffer out to the caller's output across as many `compress`
   calls as it takes; return `BufError`/`Ok` until drained.
4. On `Sync`/`Full`, emit the empty-stored-block boundary and byte-align (zenflate
   already does this for `gzip_compress_parallel`); on `Full`, also reset history.

That's a buffer and a copy, not a rearchitecture. The near-optimal levels (10–12)
inherently buffer a block before parsing anyway, so they fit this shape naturally.
The cost is latency (you hold a block before emitting) and an extra internal buffer —
the same tradeoff zlib makes.

Still new public capability on top: `deflateSetDictionary`, `set_level` mid-stream,
exact `total_in`/`total_out`, and the `Status`/`FlushCompress` mapping.

## The level-mapping question (the interesting part)

flate2's `Compression` is 0..=9. zenflate's compelling range is its near-optimal
L10–L12, which is *outside* that. A naive 1:1 map (flate2 level N → zenflate
`libdeflate(N)`) would make zenflate-as-backend **worse than zlib-rs at most levels**:
at matched level number, zenflate L2–L9 produce larger output than zlib-rs L2–L9
(zlib's lazy matching is better tuned at those levels), and only win on speed.

Example, silesia/dickens (`benchmarks/zlib_rs_compare_2026-06-18.txt`):

| level | zenflate ratio | zlib-rs ratio | zenflate c MiB/s | zlib-rs c MiB/s |
|------:|---------------:|--------------:|-----------------:|----------------:|
| 6     | 2.36x          | 2.62x         | 246              | 72              |
| 9     | 2.36x          | 2.64x         | 244              | 32              |
| 12    | 2.62x          | 2.64x (L9)    | 97               | 32 (L9)         |

At matched level, zenflate is faster but compresses worse. To match zlib-rs's L6/L9
*ratio*, zenflate needs its near-optimal L12 — which still lands at 97 MiB/s vs
zlib-rs's 32 MiB/s for the same output size. That's the honest framing: zenflate can
match zlib's ratio and stay 1.3–3x faster, but only if the level map is calibrated
to ratio, not to level number.

zenflate's effort knob (`CompressionLevel::new(effort)`, 0..=200, Pareto-ranked) is
the right lever. A calibrated 0..=9 → effort map could target zlib-rs's achieved
ratio at each level while staying on the faster side of the curve — "zlib's ratio at
every level, faster compress." Building that map is a calibration job, and per this
workspace's sweep discipline it has to cover size × level × content, not a couple of
files. It's the difference between a backend that's "faster but worse" and one that's
"same ratio, faster."

One regression to state plainly: zlib-rs has the faster *inflate* on compressible
data (dickens L6 decode 1063 vs 751 MiB/s; xml 2747 vs 1769). On near-incompressible
data zenflate's inflate is even or faster (synthetic mixed 1.3 vs 1.5 ms). Most
flate2 users decompress more than they compress, so a zenflate backend trades a
~1.2–1.5x slower decode on text for a faster encode. Whether that's a good trade
depends entirely on the workload.

## Three ways to ship

1. **Upstream PR to flate2-rs.** A `zenflate` feature + `src/ffi/zenflate.rs`,
   mirroring the zlib-rs backend. Precedent exists (zlib-rs was accepted), but it
   needs maintainer buy-in and has to pass flate2's full test suite: round-trips
   across every flush mode, tiny-output-buffer streaming, dictionary adler checks,
   gzip multi-member, exact counters. The streaming `Compress`/`Decompress` work
   above is a hard prerequisite — flate2's tests *are* the streaming contract.

2. **Fork flate2** (`flate2-zenflate` or similar). Full control, no upstream
   gatekeeping, but you own the fork and its I/O adaptors forever. Reasonable as a
   staging ground to prove the backend before proposing it upstream.

3. **A flate2-compatible surface in zenflate.** Mirror `Compress`/`Decompress`/
   `GzEncoder`/`GzDecoder` so users swap `flate2::` for `zenflate::flate2compat::`
   with a one-line change, skipping flate2 entirely. This dodges the private-trait
   problem but needs the same streaming machinery to be truly drop-in. Upside: you
   control the level mapping and can expose L10–12 directly instead of clamping to 9.

## What it would actually take

In rough order of effort, smallest first:

- Streaming `Decompress` (inflate into a caller slice + counters + dictionary +
  status/flush mapping): **small–moderate.** The state machine exists.
- `deflateSetDictionary` / `inflateSetDictionary`: **small–moderate** each side.
  zenflate's parallel path already primes the matchfinder from a 32K overlap, so the
  history-priming primitive is partly there.
- Streaming `Compress` (block-buffer wrapper + all-levels + five flush modes +
  history reset on `Full`): **moderate**, and the bulk of the work, mostly in
  getting flush semantics exactly right and tested.
- Level-map calibration so ratio matches zlib-rs per level: **moderate**, a sweep
  job under the size × level × content discipline.
- Backend module + feature wiring + conformance against flate2's suite + CI on all
  target platforms: **moderate.**

None of it needs `unsafe` or a rewrite of the core codec. The streaming compressor
is the one genuinely new subsystem, and it's worth having on its own merits — a
real `Read`/`Write` streaming encode API is a capability gap today, independent of
flate2. The pragmatic sequence is: build streaming `Compress`/`Decompress` in
zenflate with the zlib flush taxonomy first, prove it with round-trip and
tiny-buffer fuzzing, then decide between an upstream backend, a fork, or a compat
shim. The backend module is the easy 250 lines once the streaming engine underneath
it exists.

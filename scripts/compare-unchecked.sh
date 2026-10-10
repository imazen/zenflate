#!/usr/bin/env bash
# Run through run-heavy. Existing PNG harnesses compare identical input bytes.
set -euo pipefail
input_dir=${1:?PNG input directory required}
results=${2:?new results directory required}
# Coordinate with both current and legacy benchmark lanes. Opening the legacy
# lock read-only preserves it without writing scratch data outside ~/tmp.
mkdir -p "$HOME/tmp/zenbench"
exec 8>"$HOME/tmp/zenbench/zenbench.lock"
flock 8
if [[ -f /tmp/zenbench/zenbench.lock ]]; then
    exec 9</tmp/zenbench/zenbench.lock
    flock 9
fi
mkdir "$results"
results=$(realpath "$results")
git rev-parse HEAD > "$results/commit.txt"
date -u +%Y-%m-%d > "$results/date.txt"
rustc -Vv > "$results/rustc.txt"
printf 'RUSTFLAGS=%s\nCARGO_ENCODED_RUSTFLAGS=%s\n' "${RUSTFLAGS-}" "${CARGO_ENCODED_RUSTFLAGS-}" > "$results/rustflags.txt"
target_dir=${CARGO_TARGET_DIR:-target}
lscpu > "$results/cpu.txt"
lscpu -e=CPU,CORE,MAXMHZ > "$results/cpu-topology.txt"
bench_cpu=${ZENFLATE_BENCH_CPU:-2}
echo "$bench_cpu" > "$results/cpu-affinity.txt"
files=()
for id in 1207 5207 8007 8107; do
    for size in 64 256 1024 2560; do
        files+=("$input_dir/${id}_rgb8_${size}.png")
    done
done
for id in 1207 5207 8107; do
    for format in gray8 rgb16 rgba8; do
        files+=("$input_dir/${id}_${format}_1024.png")
    done
done
sha256sum "${files[@]}" > "$results/inputs.sha256"
for mode in safe unchecked; do
    args=()
    if [[ $mode == unchecked ]]; then args+=(--features unchecked); fi
    cargo build --release --locked --example png_ladder_pareto --example png_inflate "${args[@]}"
    cp "$target_dir"/release/examples/png_ladder_pareto "$results/encode-$mode"
    cp "$target_dir"/release/examples/png_inflate "$results/decode-$mode"
done
sha256sum "$results"/encode-* "$results"/decode-* > "$results/binaries.sha256"
arms=zen_png1,zen_png4,zen_png10,zen_png19,zen_png30,zen_e1,zen_e10,zen_e15,zen_e24,zen_e30,zen_ld1,zen_ld6,zen_ld12
echo "$arms" > "$results/arms.txt"
for round in 1 2 3; do
    modes=(safe unchecked)
    if [[ $round == 2 ]]; then modes=(unchecked safe); fi
    for mode in "${modes[@]}"; do
        taskset -c "$bench_cpu" "$results/encode-$mode" "${files[@]}" --rounds 3 --arms "$arms" \
            --tsv "$results/encode-$mode-$round.tsv" > "$results/encode-$mode-$round.log" 2>&1
        taskset -c "$bench_cpu" "$results/decode-$mode" "${files[@]}" --rounds 9 > "$results/decode-$mode-$round.txt" 2> "$results/decode-$mode-$round.log"
        echo "completed round $round $mode"
    done
done

# zenflate development recipes

# Run all tests (safe mode)
test:
    cargo test

# Run all tests with unchecked bounds elimination
test-unchecked:
    cargo test --features unchecked

# Run all benchmarks
bench:
    cargo bench

# Run all benchmarks with unchecked bounds elimination
bench-unchecked:
    cargo bench --features unchecked

# Run clippy (both feature sets)
clippy:
    cargo clippy --all-targets -- -D warnings
    cargo clippy --all-targets --features unchecked -- -D warnings

# Format check
fmt:
    cargo fmt --all -- --check

# Format fix
fmt-fix:
    cargo fmt --all

# Profile compression with callgrind (level and data type)
callgrind level data:
    cargo build --release --features unchecked --example compress_file 2>/dev/null || \
    cargo bench --no-run --features unchecked
    @echo "Running callgrind for L{{level}} {{data}}..."
    valgrind --tool=callgrind --callgrind-out-file=/tmp/callgrind-L{{level}}-{{data}}.out \
        cargo bench --features unchecked -- "compress/{{data}}/zenflate/L{{level}}" --profile-time 1

# Profile compression with cachegrind
cachegrind level data:
    @echo "Running cachegrind for L{{level}} {{data}}..."
    valgrind --tool=cachegrind --cachegrind-out-file=/tmp/cachegrind-L{{level}}-{{data}}.out \
        cargo bench --features unchecked -- "compress/{{data}}/zenflate/L{{level}}" --profile-time 1

# Check everything (tests + clippy + fmt)
check: fmt clippy test test-unchecked

# Build benchmarks without running (CI verification)
bench-check:
    cargo bench --no-run
    cargo bench --no-run --features unchecked

# Run a specific benchmark group
bench-group group:
    cargo bench --features unchecked -- "{{group}}"

# Run corpus benchmarks (Canterbury, Silesia, photos)
bench-corpus:
    cargo bench --bench corpus --features unchecked

# Download corpus files (Canterbury + Silesia) if not cached
corpus-download:
    #!/usr/bin/env bash
    set -euo pipefail
    CACHE="$HOME/.cache/compression-corpus"
    mkdir -p "$CACHE"
    if [ ! -d "$CACHE/canterbury" ]; then
        echo "Downloading Canterbury corpus..."
        wget -q -O "$CACHE/cantrbry.tar.gz" "https://corpus.canterbury.ac.nz/resources/cantrbry.tar.gz"
        mkdir -p "$CACHE/canterbury"
        tar xzf "$CACHE/cantrbry.tar.gz" -C "$CACHE/canterbury"
        rm "$CACHE/cantrbry.tar.gz"
        echo "Canterbury corpus ready."
    else
        echo "Canterbury corpus already cached."
    fi
    if [ ! -d "$CACHE/silesia" ]; then
        echo "Downloading Silesia corpus (~200MB)..."
        wget -q -O "$CACHE/silesia.zip" "https://sun.aei.polsl.pl//~sdeor/corpus/silesia.zip"
        mkdir -p "$CACHE/silesia"
        cd "$CACHE/silesia" && unzip -o ../silesia.zip && rm ../silesia.zip
        echo "Silesia corpus ready."
    else
        echo "Silesia corpus already cached."
    fi

# Fuzz decompression with arbitrary input (default 60s)
fuzz-decompress seconds="60":
    cargo +nightly fuzz run fuzz_decompress -- -max_total_time={{seconds}} -max_len=65536

# Fuzz compress+decompress round-trip (default 60s)
fuzz-roundtrip seconds="60":
    cargo +nightly fuzz run fuzz_roundtrip -- -max_total_time={{seconds}} -max_len=65536

# Build fuzz targets without running
fuzz-check:
    cargo +nightly fuzz build

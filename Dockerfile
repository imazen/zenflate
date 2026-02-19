FROM rust:latest

# Install profiling tools
RUN apt-get update && apt-get install -y \
    valgrind \
    linux-perf \
    && rm -rf /var/lib/apt/lists/*

# Install just (task runner)
RUN cargo install just

WORKDIR /zenflate
COPY . .

# Pre-build benchmarks in release mode
RUN cargo bench --no-run --features unchecked

# Default: run the benchmark suite
CMD ["cargo", "bench", "--features", "unchecked"]

# Every recipe is a plain cargo call: the toolchain, linker and profile shape
# come from `rust-toolchain.toml`, `.cargo/config.toml` and `Cargo.toml`, so a
# hand-typed `cargo test` behaves identically to `just test`. Nothing here sets
# RUSTFLAGS — that would silently drop the workspace flags (see ADR-0029).
#
# Test lanes follow CLAUDE.md "Test-runtime policy": the default lane is every
# test on the fake clock (paused tokio) or fast on the real clock; anything
# real-clock >60 s is `#[ignore]`d into the slow lane.

default:
    @just --list --unsorted

# ── inner loop ─────────────────────────────────────────────────────────

# Type-check everything, tests included — no codegen, no linking.
check:
    cargo check --workspace --all-targets

# Compile libraries and binaries (no test targets).
build:
    cargo build --workspace

# Default lane. Optional filter: `just test cancel_hold`.
test filter='':
    cargo test --workspace {{ filter }}

# Slow lane: the real-clock >60 s tests `#[ignore]`d out of `test`.
test-slow:
    cargo test --workspace --release -- --ignored

# Both lanes.
test-all: test test-slow

# ── review gates ───────────────────────────────────────────────────────

lint:
    cargo clippy --workspace --all-targets

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# ── deploy ─────────────────────────────────────────────────────────────

# Build the k8s image: every runner binary, `--release`, fully optimized.
image tag='siprustserver:dev':
    docker build -f deploy/docker/Dockerfile -t {{ tag }} .

# ── environment ────────────────────────────────────────────────────────

# Check this machine can build the workspace (toolchain, mold, a real compile).
doctor:
    @echo "toolchain: $(rustc -vV | sed -n 's/^release: //p') ($(rustup show active-toolchain | cut -d' ' -f1))"
    @command -v mold >/dev/null \
        && echo "mold:      $(mold --version | cut -d' ' -f2)" \
        || { echo "mold:      MISSING -> apt-get install mold"; exit 1; }
    @cargo build -p sip-clock --quiet && echo "build:     ok"

# ── disk ───────────────────────────────────────────────────────────────

# What the build tree currently costs.
disk:
    @du -sh target/* 2>/dev/null | sort -rh

# Drop incremental state, keep compiled artifacts — cheapest GB back mid-session.
clean-incremental:
    rm -rf target/debug/incremental target/release/incremental

# Drop the dev tree, keep `--release` (slow lane + local bench binaries need it).
clean-dev:
    cargo clean --profile dev

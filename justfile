# Every recipe is a plain cargo call: the toolchain, linker, jobs cap and
# profile shape come from `rust-toolchain.toml`, `.cargo/config.toml` and
# `Cargo.toml`, so a hand-typed `cargo nextest run` or `cargo test` builds
# exactly what `just test` builds — bounded to 4 jobs, which is what keeps a
# whole-workspace link from exhausting memory (ADR-0029 X5). Nothing here sets
# RUSTFLAGS or --jobs: the first would silently drop the workspace flags, the
# second raise the cap.
#
# The test lanes run under cargo-nextest (`.config/nextest.toml`): one process
# per test, 16 at once across every binary. nextest skips doc tests, so
# `just test` runs them with `cargo test --doc`.
#
# Test lanes follow CLAUDE.md "Test-runtime policy": the default lane is every
# test on the fake clock (paused tokio), whatever its length, and every
# real-clock test under 1 s; real-clock tests of 1 s or more and every loadgen
# test are `#[ignore]`d into the slow lane. One crate's tests: `cargo nextest
# run --workspace -E 'package(=<crate>)' <filter>`, which reuses the workspace
# build (a `-p` build resolves other features and recompiles). A filtered run
# skips an ignored test without a word: add `--run-ignored all` (`cargo test`:
# `-- --include-ignored`).

default:
    @just --list --unsorted

# ── inner loop ─────────────────────────────────────────────────────────

# Type-check everything, tests included — no codegen, no linking.
check:
    cargo check --workspace --all-targets

# Compile libraries and binaries (no test targets).
build:
    cargo build --workspace

# Default lane. Optional filter: `just test cancel_hold`. Both lanes first check
# the one-test-binary-per-crate layout (ADR-0030, `test-binaries.allow`).
test filter='':
    scripts/check-test-layout.sh
    cargo nextest run --workspace --no-tests=warn {{ filter }}
    cargo test --doc --workspace {{ filter }}

# Slow lane: the real-clock >= 1 s and loadgen tests `#[ignore]`d out of `test`,
# under nextest's `slow` profile in a release build.
test-slow:
    scripts/check-test-layout.sh
    cargo nextest run --workspace --release -P slow --run-ignored only

# Both lanes.
test-all: test test-slow

# The live ELU under a real one-core cgroup CPU quota (a transient systemd
# user scope): the one `overload::quota_tests` case no other lane can run.
test-cpu-quota:
    cargo test -p b2bua --lib --no-run
    systemd-run --user --wait --pipe --quiet --same-dir -p CPUQuota=100% -E PATH -E HOME \
        cargo test -p b2bua --lib -- --ignored --exact overload::quota_tests::live_signal_sheds_under_a_real_one_core_quota

# ── review gates ───────────────────────────────────────────────────────

lint:
    cargo clippy --workspace --all-targets

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# Rustdoc of every workspace crate, warnings denied: a broken or private
# intra-doc link, or unclosed HTML in prose, fails. A bin named like its
# package's lib is skipped by `cargo doc`, so each one gets its own
# `cargo rustdoc`; its page overwrites the lib's index.html under target/doc.
doc-check:
    #!/usr/bin/env bash
    set -uo pipefail
    export RUSTDOCFLAGS="-D warnings"
    rc=0
    cargo doc --no-deps --workspace --keep-going || rc=1
    while read -r pkg bin; do
        cargo rustdoc -p "$pkg" --bin "$bin" || rc=1
    done < <(cargo metadata --no-deps --format-version 1 | jq -r '.packages[]
        | ([.targets[] | select(.kind | index("lib")) | .name]) as $libs
        | .name as $pkg | .targets[] | select(.kind | index("bin"))
        | select((.name | gsub("-"; "_")) as $n | $libs | index($n)) | "\($pkg) \(.name)"')
    exit "$rc"

# Point git at the versioned hooks (pre-commit: staged Rust must be fmt-clean;
# prepare-commit-msg: no amend on develop). A hooks path already set is left
# alone: an embedding checkout may run its own hooks that call these.
hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    if current="$(git config --local --get core.hooksPath)"; then
        echo "hooks: core.hooksPath is already $current; left as is" >&2
    else
        git config core.hooksPath .githooks
    fi

# ── deploy ─────────────────────────────────────────────────────────────

# Build the k8s image: every runner binary, `--release`, fully optimized.
image tag='siprustserver:dev':
    docker build -f deploy/docker/Dockerfile -t {{ tag }} .

# Loopback check of the lane SIPp scenarios against in-dialog OPTIONS peers (needs `sipp`).
sipp-check:
    deploy/k8s/sipp/checks/run.sh

# The SIPp stat exporter against a large stat CSV under a memory cap, its disk trimmer
# and its error-file follower across SIPp's rotation (the last needs `sipp`).
sipp-exporter-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s deploy/k8s/sipp/exporter-check

# ── environment ────────────────────────────────────────────────────────

# Check this machine can build the workspace (toolchain, mold, a real compile).
doctor:
    @echo "toolchain: $(rustc -vV | sed -n 's/^release: //p') ($(rustup show active-toolchain | cut -d' ' -f1))"
    @command -v mold >/dev/null \
        && echo "mold:      $(mold --version | cut -d' ' -f2)" \
        || { echo "mold:      MISSING -> apt-get install mold"; exit 1; }
    @command -v jq >/dev/null \
        && echo "jq:        $(jq --version)" \
        || { echo "jq:        MISSING -> apt-get install jq (scripts/check-test-layout.sh)"; exit 1; }
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

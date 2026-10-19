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

# The no-panic property lanes (ADR-0051 §8): the slow-lane tests of every
# module named `no_panic_lane`, run in the test profile so overflow checks and
# debug assertions hold while they run.
property_lane := "test(/(^|::)no_panic_lane::/)"

# Slow lane: the real-clock >= 1 s, loadgen and property-lane tests `#[ignore]`d
# out of `test`, under nextest's `slow` profile, in a release build but for the
# property lanes. Optional nextest filterset: `just test-slow 'package(=loadgen)'`
# runs one crate's slow lane.
test-slow *filter:
    scripts/check-test-layout.sh
    cargo nextest run --workspace --release -P slow --run-ignored only --no-tests=warn -E {{ quote("(not " + property_lane + ")" + if filter == "" { "" } else { " & (" + filter + ")" }) }}
    cargo nextest run --workspace -P slow --run-ignored only --no-tests=warn -E {{ quote(property_lane + if filter == "" { "" } else { " & (" + filter + ")" }) }}

# Both lanes.
test-all: test test-slow

# The parity oracle (ADR-0001): sip-message's test targets built under its
# `rvoip-oracle` feature, which no workspace build turns on, and its
# differential tests run. A branch touching sip-message passes it.
parity-oracle:
    cargo nextest run -p sip-message --features rvoip-oracle --no-tests=fail -E 'test(/^rvoip_parity::/)'

# The live ELU under a real one-core cgroup CPU quota (a transient systemd
# user scope): the one `overload::quota_tests` case no other lane can run.
test-cpu-quota:
    cargo test -p b2bua --lib --no-run
    systemd-run --user --wait --pipe --quiet --same-dir -p CPUQuota=100% -E PATH -E HOME \
        cargo test -p b2bua --lib -- --ignored --exact overload::quota_tests::live_signal_sheds_under_a_real_one_core_quota

# ── review gates ───────────────────────────────────────────────────────

lint: file-size purity panic-sites audit-gate
    cargo clippy --workspace --all-targets

# The RustSec audit of Cargo.lock (scripts/audit.py, .cargo/audit.toml): a
# vulnerability not ignored there fails when the branch touches Cargo.lock or
# .cargo/audit.toml, and is a warning otherwise.
audit-gate:
    scripts/audit.py

# The full `cargo audit` report, then the outdated direct dependencies
# (`cargo upgrade --dry-run --incompatible allow`); it prints, it never fails.
audit:
    @scripts/audit.py --report

# The audit script on throwaway repositories, one real cargo-audit run on a
# vulnerable lockfile included (under a few seconds).
audit-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/audit-check

# File-size budget: a first-party .rs file has at most 500 lines, a test file
# 800, a file `file-size.allow` lists at most its listed size
# (scripts/file-size.py). The pre-commit hook checks the staged files.
file-size:
    scripts/file-size.py

# Lowers each allowlist entry to its file's size and removes the entries of
# files under the limit; never raises nor adds one.
file-size-update:
    scripts/file-size.py --update

# Every file over its limit, largest first, with its budget.
large-files:
    @scripts/file-size.py --report

# The file-size lint on throwaway repositories, this repository's settings
# included (under a second).
file-size-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/file-size-check

# The tiers of the production closure (scripts/purity.py, purity.toml,
# ADR-0051 §1): each production binary's closure, built alone, reaches no
# `testkit`/`test-util` feature and no first-party crate without a tier, and
# the listed Dockerfiles build it by a cargo command of its own; a pure crate
# depends on pure crates and `pure-deps` only; every member's lint table is the
# workspace's. The pre-commit hook checks the staged known entries.
purity:
    scripts/purity.py

# The panic-sites ratchet (scripts/panic-sites.py, panic-sites.allow,
# ADR-0051 §2-§3): one clippy pass with the panic set over the tiered crates;
# a file holds at most its listed count, a site inside the branch's hunks
# fails, a crate at zero carries the deny attribute. The pre-commit hook checks
# the staged allowlist.
panic-sites:
    scripts/panic-sites.py

# Lowers each entry to its file's count and removes the entries at zero; raises
# or adds one only by the sites moved into its file or revealed by a rule the
# base did not count.
panic-sites-update:
    scripts/panic-sites.py --update

# The counts per crate, then every file with a site in allowlist form.
panic-sites-report:
    @scripts/panic-sites.py --report

# Both checks on throwaway cargo workspaces, one real clippy pass under this
# repository's clippy.toml included (a few seconds).
purity-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/purity-check

# Rewrites every file outside Rust that restates the RFC rule list (the token
# fixture, the TypeScript `RFC_RULES`) from the rule registry. pivot-schema's
# `every_rule_list_copy_is_current` names a stale one.
rule-copies:
    cargo run -q -p pivot-schema -- rule-copies --write

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

# The helm charts (deploy/helm): lint, the render contracts, the extension
# points through a downstream chart, the kind values against net-env.sh, the
# lab's SUT helpers and the golden renders (needs `helm` and PyYAML, a few
# seconds).
helm-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s deploy/helm/check

# Rewrites the golden renders of the kind values from the current charts.
helm-golden:
    HELM_GOLDEN_UPDATE=1 PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s deploy/helm/check -k Golden

# The SIPp stat exporter against a large stat CSV under a memory cap, its disk trimmer
# and its error-file follower across SIPp's rotation (the last needs `sipp`).
sipp-exporter-check:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s deploy/k8s/sipp/exporter-check

# The HA tooling runner (tooling/hakit, ADR-0049): pyright strict and its tests
# under the toolchain its uv lockfile pins (needs `uv`, and `sipp` for the
# record checks, which fail without it).
hakit-check:
    cd tooling/hakit && uv run --locked pyright && uv run --locked pytest

# ── environment ────────────────────────────────────────────────────────

# Check this machine can build the workspace (toolchain, mold, a real compile);
# the audit tools are reported, a missing one does not fail.
doctor:
    @echo "toolchain: $(rustc -vV | sed -n 's/^release: //p') ($(rustup show active-toolchain | cut -d' ' -f1))"
    @command -v mold >/dev/null \
        && echo "mold:      $(mold --version | cut -d' ' -f2)" \
        || { echo "mold:      MISSING -> apt-get install mold"; exit 1; }
    @command -v jq >/dev/null \
        && echo "jq:        $(jq --version)" \
        || { echo "jq:        MISSING -> apt-get install jq (scripts/check-test-layout.sh)"; exit 1; }
    @command -v cargo-audit >/dev/null \
        && echo "audit:     $(cargo audit --version | cut -d' ' -f2)" \
        || echo "audit:     MISSING -> cargo install cargo-audit (just audit, just audit-gate)"
    @command -v cargo-upgrade >/dev/null \
        && echo "upgrade:   $(cargo upgrade --version | cut -d' ' -f2)" \
        || echo "upgrade:   MISSING -> cargo install cargo-edit (just audit's outdated report)"
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

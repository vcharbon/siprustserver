# Lanes and build

[CLAUDE.md](../../CLAUDE.md) keeps the directives; this page holds their detail. Read before choosing a
lane for a test, adding an integration test, or changing how the workspace builds.

## Test-runtime policy (default and slow lane)

**The default lane holds every fake-clock test, whatever its length, and every
real-clock test under 1 s. Nothing else runs by default.** A test is fake-clock
when its runtime is paused (`start_paused`, `sip_clock::run_paused_within`);
any other test, synchronous ones included, is real-clock. A real-clock test of
1 s or more (worst run at 16 test threads) is
`#[ignore = "slow lane: real clock >= 1 s"]`, and every `loadgen` test, lib and
integration alike, is `#[ignore = "slow lane: loadgen"]`. Lanes live in the
`justfile` and run under cargo-nextest (`.config/nextest.toml`: one process per
test, 16 at once across every binary; doc tests through `cargo test --doc`):
`just test` (default), `just test-slow [filterset]` (`cargo nextest run --release
-P slow --run-ignored only [-E filterset]`; `just test-slow 'package(=loadgen)'`
runs one crate's slow lane). One crate's tests:
`cargo nextest run --workspace -E 'package(=<crate>)' <filter>`, which reuses
the workspace build (a `-p` build resolves other features and recompiles). A
filtered run skips an ignored test without a word: add `--run-ignored all`
(`cargo test`: `-- --include-ignored`) to run one. Moving a real-clock scenario out is the
moment to write its paused-clock equivalent for the default lane. A slow
paused-clock test stays in the default lane: cut its timer churn at the source
(clock guide, rule 5).

## Compiling ([ADR-0029](../adr/0029-dev-build-cost.md))

`just` is the entry point, run it bare to list the lanes. `just check` for the
fast signal (no codegen), `just test [filter]` / `just test-slow [filterset]`, `just lint`,
`just image` for the k8s image, `just doctor` when a machine looks broken,
`just disk` / `just clean-incremental` under disk pressure. Every recipe is a
plain cargo call, so a hand-typed `cargo nextest run` or `cargo test` builds
identically, including
the parallelism cap: `.cargo/config.toml` sets `[build] jobs = 4`, because this
workspace links ~250 test binaries and the concurrent mold links at one job per
core exhaust a 32 GB host (ADR-0029 X5). `--jobs N` overrides it per call:
lower on a shared host, never higher for a whole-workspace run. A host that wraps cargo in a jobserver states its own bound, and `--jobs` may then be ignored.

Prerequisites: mold and gcc >= 12; for the dependency audit (`just audit`,
`just audit-gate`), cargo-audit and cargo-edit (`cargo upgrade`), cargo-outdated
optionally (on Arch: `pacman -S cargo-audit cargo-edit cargo-outdated`). Do not:

- set `RUSTFLAGS`, it replaces `[build] rustflags` wholesale, silently
  dropping `tokio_unstable` *and* mold; anything that must set it re-states
  both flags;
- change `profile.dev`'s codegen backend, Cranelift is rejected (ADR-0029 X3),
  and re-proposing it means re-running the three panic tests named there;
- put profile settings in `.cargo/config.toml`, profile shape lives in
  `Cargo.toml`, toolchain/linker wiring in `.cargo/config.toml`.

## Adding an integration test ([ADR-0030](../adr/0030-one-integration-test-binary-per-crate.md))

A crate's integration tests compile into ONE binary. In a crate that has a
`tests/it/` directory, a new test file goes **inside it**, with a `mod` line
added to `tests/it/main.rs`; a stray `tests/foo.rs` links a second copy of the
whole dependency graph. Select one with
`cargo nextest run --workspace -E 'package(=<crate>) & binary(it)' <module>::<name>`.

A test that installs process-global state (a trace registry, the allocation
counter, a real socket, process env) stays a `tests/*.rs` target of its own,
folded in, it does not fail the suite, it wedges it.

## Agent and build concurrency

**By default two agents (subagent / workflow stage) compile or run tests at
once;** other work that is not heavy on disk, CPU or memory runs without limit.
Builds serialise on the target directory and a build racing a running SUT or
loadgen skews what the run measures. The jobs cap in
`.cargo/config.toml` is sized for a quiet host; beside other builds lower it
(`cargo test --workspace --jobs 2`) and, on a host short on memory, cap the
command in a scope: `systemd-run --user --scope -q -p MemoryMax=12G cargo …`; a
long-lived test process (SUT, loadgen, e2e-web) then gets its own small scope.

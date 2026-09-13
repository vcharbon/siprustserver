# The dev build: mold links it, dependencies carry no debuginfo, LLVM compiles it

**Status:** accepted (2026-08-15)

## Context

The workspace's dev build is dominated by its test tree, not by its production
code: 39 packages, but 188 integration-test targets — and Cargo links one
binary per target, each statically pulling the whole dependency graph.
`target/debug` stood at 6.3 GB (3.5 GB `deps/`, 2.6 GB incremental state) on a
WSL2 box that runs at ~93 % disk.

Cold builds of `-p failover-harness -p b2bua-harness --tests` (117 crates, 85
test binaries, 8 GB / 16-core box):

| build                                          | cold   | `target/` |
| ---------------------------------------------- | ------ | --------- |
| stable, LLVM, GNU ld, deps keep debuginfo      | 26.7 s | 6047 MB   |
| stable, LLVM, mold, deps `debug = 0`           | 25.2 s | 5260 MB   |
| nightly, LLVM, mold, deps `debug = 0`          | 21.6 s | 5945 MB   |
| nightly, **Cranelift**, mold, deps `debug = 0` | 21.7 s | 6550 MB   |

Cranelift was the obvious candidate and it does not survive contact with this
workspace — see X3. The structural fix is ADR-0030, which is worth an order of
magnitude more than anything in this table.

## Decision

### X1 — mold is a build requirement, not a local preference

`-Clink-arg=-fuse-ld=mold` sits in `[build] rustflags`, so it applies to dev
and release alike, and `deploy/docker/Dockerfile` installs mold in the builder
stage. Linking is a first-order cost in a workspace that links this many
binaries; leaving the linker to each machine would make build times
unreproducible and leave the deploy image on the slowest option. `just doctor`
fails loudly when mold is absent.

### X2 — Dependencies carry no debuginfo

`[profile.dev.package."*"] debug = false`. First-party crates keep
`debug = "line-tables-only"` — the `PanicDump` wire-trace dump and ordinary
test triage read first-party frames, and a dependency's `file:line` is never
the thing acted on. Dependency rlibs are the bulk of the bytes every test
binary links: 13 % of the dev tree on its own.

### X3 — LLVM compiles every profile; Cranelift is rejected

Cranelift **miscompiles the panic-containment path**. Under
`codegen-backend = "cranelift"` the three tests that drive a deliberate panic
through the SUT's catch — `decision_deadline::panicking_decision_is_rejected_
503_immediately`, `reaper::handler_panic_strike1_reaps_via_rules`,
`reaper::second_panic_discharges_outside_the_rules` — fail instantly with the
panic escaping; the identical binary built with LLVM passes all three. Panic
containment is load-bearing SUT behaviour (the reaper's strike ledger, the
decision adapter's 503), so a backend that changes it cannot compile the lane
that certifies it, at any speed.

The table above says it would not have been worth it regardless: against
nightly LLVM, Cranelift was 0.1 s slower and 605 MB larger. Its win is in
codegen, and codegen is not this workspace's constraint — linking is.

### X4 — The toolchain is stable, and declared

`rust-toolchain.toml` pins `channel = "stable"`. With Cranelift rejected there
is no reason to reach for nightly, whose only remaining offer was ~14 % of a
cold build that ADR-0030 then made three times smaller. Declaring the channel
is still worth a file: it is what stops a dependency that ships its own
`rust-toolchain.toml` inside its crate tarball from choosing the compiler.

### X5 — Cargo's parallelism is capped in config, and the cap is the default

`[build] jobs = 4` in `.cargo/config.toml`, with `-Wl,--thread-count=4` on
every mold invocation. Peak build memory scales with the jobs in flight, and
the workspace links ~250 test binaries: at cargo's default of one job per core,
a 24-core / 32 GB host ran out of memory mid-link — mold reported `failed to
write to an output file. Disk full?`, which is its SIGBUS handler on the
mmap'd output, and dumped core in the crate root (10 GB apiece). `--jobs 4`
completed the same cold run; `--jobs 6` failed once other builds held half the
host. mold's own default of one thread per core multiplied that: `jobs` links
in flight ran `jobs * ncpu` linker threads over the same page cache.

A config cap is the only place the bound holds for a hand-typed `cargo test`,
which CLAUDE.md promises behaves like `just test`; a `--jobs` in a recipe would
protect the recipe alone. Cargo cannot size it to free memory, so 4 is the
quiet-host value and the command line goes downward from it on a shared host.
Verified cold on the box above: the whole test tree built in 1m32s, at most 4
links in flight, the largest mold at 690 MB RSS and all of them under 1.2 GB
together, rustc under 1.8 GB — against ~17 GB of linkers alone at one job per
core.

## Consequences

- mold and a gcc >= 12 are hard prerequisites. `just doctor` reports both.
- A whole-workspace build takes ~4 cores however many the host has; `--jobs N`
  raises it for a single call only when the host is quiet and has the memory.
- `RUSTFLAGS` in the environment silently replaces `[build] rustflags` whole —
  losing `tokio_unstable` *and* mold. Nothing in the repo sets it; anything
  that does must re-state both flags.
- Re-evaluating Cranelift means re-running the three panic tests first. If a
  future release fixes them, the table in X3 is the bar it then has to clear.

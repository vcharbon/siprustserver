# Coverage-guided fuzzing

The no-panic property lanes (ADR-0051 §8) mutate fixed seeds by the protocol's structure and
never learn which inputs reach new code. Coverage-guided fuzzing does: libFuzzer keeps every
input that reaches a new edge and mutates from it. It runs in its own workspace (`fuzz/`, a
pinned nightly), as **discovery only**: it is never a gate, `just verify` never builds it,
and a finding counts once it is a committed regression test in the owning crate.

## When to run it

After a change to a crate a target covers, before the branch merges, when the change touches
how that crate reads peer input: a parser, a decoder, a header read, the SDP answer, the
turn's rule chain, the proxy's datagram path. A run of 10 to 60 minutes on the targets of
the touched crates is the normal size; a campaign (hours per target, several processes) is
for a new target or a rewrite of a reader. Never as a CI gate: a run's outcome depends on
time and luck.

## Targets

Ordered by what a panic costs (ADR-0051 §5): `just fuzz-list` prints them in this order.

| Target | Drives | A panic there |
|---|---|---|
| `b2bua_turn` | a turn-lane script over the dialled call, one step's SIP text replaced by the input; two runs agree | tears the call down |
| `sdp_answer` | every SDP read, answer and restatement of the message lane's probe, over two descriptions | tears the call down (the answer is built in a turn) |
| `proxy_datagram` | one proxy core on the simulated fabric, the input sent twice from a seed's peer | restarts the recv loop every call shares |
| `repl_frame` | the replication stream reader and frame decoder; a decoded frame writes back to itself | restarts the replication task |
| `call_snapshot` | the call body decode; a decoded call writes back to itself | restarts the replication task |
| `message_roundtrip` | every entry point of the message lane's probe, and parse → render → parse: the rendering reads back to itself and passes the write check | drops one datagram (contained) |
| `multipart` | a multipart body under its boundary, read and composed again | drops one datagram |
| `credentials` | each line as each header of the authentication family; the rendering reads back to itself and passes the write check | drops one datagram |

A target compiles in the driver and seeds of its no-panic lane from the crate's own test tree
(`fuzz/lanes`), so it drives what the lane drives. A change to a lane driver's signature
breaks the fuzz build, which nothing else builds: check it in the same change with
`cd fuzz && cargo check --workspace --bins`.

## Running

Prerequisite: `cargo install cargo-fuzz`; the nightly of `fuzz/rust-toolchain.toml` installs on
first use.

```bash
just fuzz b2bua_turn --minutes 60 --jobs 8   # fuzz one target
just fuzz-min b2bua_turn                     # minimise its corpus
just fuzz-cov b2bua_turn                     # line coverage its corpus reaches, per file
```

`scripts/fuzz.sh` holds the details (its header lists every command and variable). A run
first rewrites the lanes' seeds under `fuzz/seeds-out`, grows its corpus under `fuzz/corpus`
(`FUZZ_CORPUS` moves it), and reads more seed corpora from `FUZZ_SEEDS`; seeds taken from
real traffic belong in such a directory, outside this repository.

What a run checks beside panics:

- **Superlinear time**: an input that runs past `-timeout` (10 s, `FUZZ_TIMEOUT`) is kept as
  a timeout; inputs grow to 64 KiB, so work quadratic in the input crosses it.
- **Allocations sized from input**: `-rss_limit_mb=1024` per process, and the targets' own cap on one
  allocation (`fuzz/lanes/src/alloc_cap.rs`), in place of `-malloc_limit_mb`, which needs a
  sanitizer's malloc hooks. The build is sanitizer-free: the workspace forbids `unsafe`
  code, and its audited exceptions are socket calls the simulated fabric never makes.
- **Arithmetic overflow and debug assertions**: on, as in the lanes.
- **Recursion driven by input**: each case runs on the lanes' small stack.

With `--jobs J` the run forks J processes; a crash, timeout or OOM is kept under
`fuzz/artifacts/<target>/` and the run goes on. cargo-fuzz builds the target; the fuzzer runs
outside cargo, so a cargo wrapper's limits see only the build.

## A finding

1. Reproduce it: `scripts/fuzz.sh repro <target> fuzz/artifacts/<target>/<file>` shows its panic;
   `scripts/fuzz.sh tmin <target> <file>` shrinks it to `<file>.min`.
2. Write it as a unit test that fails on the input, in the owning crate's default lane: beside
   the code it exercises, or in the crate's lane regressions where it has them
   (`sip-message`'s `no_panic_lane/regressions.rs`). An input grown from real traffic
   (a `FUZZ_SEEDS` corpus) is restated with synthetic values first: names, numbers and
   addresses of the documentation ranges, so nothing of the traffic reaches the test.
3. Fix it in the same change. A finding that needs a design change gets its own ticket, with
   the input attached.

The instrumented builds take about 3 GB under `fuzz/target`; `just disk` lists it.

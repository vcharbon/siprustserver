# `loadgen` — SIP load generator (a SIPp substitute)

`loadgen` drives **managed-rate SIP load** through a SUT (our front-proxy +
B2BUA, or any SIP element) by **reusing the functional `scenario-harness`
choreography** as load scenarios. One process, thousands of concurrent calls,
bounded memory, a live Prometheus `/metrics` endpoint, and an on-disk callflow
report that keeps the first-N samples per `(scenario × result-class)` — including
the OK flows and, for failures, *why* they failed.

It multiplexes every dialog over a **few static UDP sockets** (one per defined
endpoint: `uac`, `uas`, `refer`), so call rate is not bounded by fds/ephemeral
ports. Calls are correlated **header-only**: each call carries one random token
in `X-Loadgen-Id`, which a transparent SUT relays onto every downstream leg.

---

## Quick start

There are two ways to run it. **Start with the in-process tests** — they need no
cluster and validate the whole pipeline deterministically.

### 1. In-process (no cluster) — the smoke suite

The smoke tests stand up an **in-process `B2buaCore` SUT** on real loopback UDP
and drive the load driver against it:

```bash
# all loadgen smoke tests (correlation/demux, no-leak, orphans, picker,
# emergency-under-overload, post-call cleanup across failure modes)
cargo test -p loadgen --test smoke

# one test, with its full SIP trace printed:
cargo test -p loadgen --test smoke loadgen_mux_emergency_split_under_overload -- --nocapture
cargo test -p loadgen --test smoke loadgen_post_call_cleanup_no_leak -- --nocapture
```

These run in the **default test lane** (`just test`) — they are fast and require
nothing external.

### 2. Host → real cluster — the `loadgen` binary

Build the binary and point it at the front-proxy VIP. The mux endpoints bind on a
host IP **reachable from the cluster pods** (the kind bridge gateway, *not*
`127.0.0.1`), and `--route-pin-to-uas` tells our B2BUA to send the callee leg
back to the host `uas` socket:

```bash
cargo build --release -p loadgen

./target/release/loadgen \
  --target 172.20.255.250:5060 \      # front-proxy VIP
  --bind-ip 172.20.0.1 \              # kind bridge gateway (pods reach the host here)
  --route-pin-to-uas \                # X-Api-Call pin: SUT routes b-leg → host uas/refer
  --scenario basic_call=4 --scenario reinvite=2 --scenario refer=1 \
  --cps 50 --duration 60 \
  --out-dir ./loadgen-report
```

Prerequisites for the real cluster (one-time):

- The B2BUA must be **transparent to the correlation header**: deploy it with
  `B2BUA_RELAY_HEADERS=X-Loadgen-Id` (already set in
  `deploy/k8s/manifests/20-worker.yaml`). Without it the callee leg never
  correlates and you'll see `loadgen_mux_orphan_total` climb / zero OK calls.
- `--bind-ip` = the kind bridge gateway:
  `docker network inspect kind -f '{{(index .IPAM.Config 0).Gateway}}'` (e.g.
  `172.20.0.1`). See the `cluster-nat-inventory` notes for the NAT details.

Key flags: `--cps`, `--duration`, `--max-in-flight`, `--target`, `--bind-ip`,
`--base-port` (uac=base, uas=base+1, refer=base+2), `--correlation-header`
(default `X-Loadgen-Id`), `--route-pin-to-uas`, `--scenario name=weight`
(repeatable; omit for the default mix), `--out-dir`, `--metrics-addr`
(default `0.0.0.0:9300`), `--sample-cap`. Run `--help` for the full list.

---

## Where the results are (and *why* calls failed)

The report is written to `--out-dir`, bucketed per `(scenario × result-class)`:

- **`index.html`** — counts table (`scenario | class | count | sample-links`),
  OK rows green, failing rows red; plus latency percentiles and checkpoints.
- **`callflows/<scenario>/<class>/<i>.html`** — the per-call **SIP sequence
  diagram** for sampled calls. For a failing call the page shows `FAIL` **and the
  reason** (the `StepError` / outcome) as the header banner and a `call-result`
  anomaly — e.g. *"alice expected 200, got 486 Busy Here"*,
  *"transfer declined by charlie (603)"*. The failure `<class>` is the directory
  name: `status_503`, `status_486`, `timeout`, `unexpected`, `rfc_audit_fail`,
  `panic`, `transport`, `unparseable`.
- **`summary.md`** — the same counts in markdown.
- **Live:** `curl <metrics-addr>/metrics` during a run for the per-`(scenario,
  class)` counters plus the `loadgen_mux_orphan_total` / `loadgen_mux_registry_size`
  canaries.

Sampling is bounded: a small fraction of calls record their trace (`--sample-cap`
per bucket); the rest are counted only. A non-sampled failure still gets a stub
page with its one-line reason.

---

## How it relates to the existing tests

- **It reuses the functional choreography.** A load scenario drives a full call
  with the *fallible* (`try_*`) variants of the same `scenario-harness` `Agent`
  methods the functional tests use — so an expected failure is a counted
  `StepError`, never a panic. The non-`Send` `Harness` wrapper is replaced by a
  `Send` `AgentBinder` (`scenario-harness/src/loadbind.rs`) so thousands of calls
  run as ordinary tokio tasks. Recording + the RFC 3261/3262/3264 audit are the
  **same** decorators the harness report uses, layered per-sampled-call.
- **The smoke suite is the regression gate.** `crates/loadgen/tests/smoke.rs`
  runs the driver against an in-process `B2buaSut` and asserts correlation/demux,
  no dialog mixing, no mux/SUT leak, orphan observability, the multi-receiver
  picker, the emergency/overload 503-split, and post-call cleanup across every
  teardown path. These are real-clock but short, so they live in the **default
  lane** (`just test`). Keep them green; they have caught real B2BUA bugs (e.g.
  the Tier-3 overload-shed per-call-lock leak).
- **It does not replace the conformance tests.** Strict per-message RFC oracles
  live in `b2bua-harness` (e.g. `refer_allow.rs`). Load scenarios are
  interleaving-tolerant on purpose — a load tool must be robust to reordering.

---

## How to add a test case

### Add a load scenario

1. Create `src/scenarios/<name>.rs` with a unit struct implementing
   `LoadScenario`:

   ```rust
   pub struct MyFlow;

   #[async_trait]
   impl LoadScenario for MyFlow {
       fn id(&self) -> ScenarioId { "my_flow" }      // report dir + metrics label
       // fn needs_charlie(&self) -> bool { true }    // bind a transfer-target leg
       // fn emergency(&self) -> bool { true }        // stamp Resource-Priority: esnet.0

       async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx)
           -> Result<(), StepError>
       {
           // Reuse the shared building blocks where you can:
           let mut dialog = establish(env, scope, ctx).await?;   // INVITE/180/200/ACK
           // ... the interesting middle ...
           hangup(env, scope, &mut dialog, ctx).await             // BYE/200
       }
   }
   ```

   - `env` gives you the bound agents (`env.alice` UAC, `env.bob` UAS, optional
     `env.charlie`), `env.via` (the SUT to route through), `env.prepare_invite`
     (stamps the correlation token + optional routing pin), and the REFER helpers.
   - **Register your dialog state in `scope`** as the call progresses
     (`set_early` once the INVITE is out, `set_confirmed` once it answers,
     `mark_terminated` once you tear it down) so a mid-flow failure is still
     cleaned up by the driver — this is what keeps the SUT leak-free.
   - `ctx.checkpoint("name")` records a latency checkpoint (shows in the report).

2. Register it in `src/scenarios/mod.rs`: add `pub mod <name>;`, a `by_id` arm
   (`"my_flow" => Some(Arc::new(<name>::MyFlow))`), and — if it should run in the
   default mix — a weight in `default_scenarios()`. An **emergency variant** is
   free: `AsEmergency::wrap("my_flow_em", Arc::new(<name>::MyFlow))`.

### Add a *voluntarily-failing* scenario (post-call-cleanup coverage)

Failure scenarios live in `src/scenarios/failures.rs`, one per teardown path, so
the no-leak coverage test exercises every reclamation branch **without an
endurance run**:

| ends in scope state | teardown the driver runs | example |
|---|---|---|
| `Terminated` (final received) | none | `InviteReject` (callee 486) |
| `Early` (no final) | CANCEL | `AbandonRinging` (caller quits on 180) |
| `Confirmed` | BYE | `ReferCharlieReject` (transfer 603) |

Return a `StepError` describing the failure (it becomes the report `detail` and
the NOK callflow banner). If a real final (`status >= 200`) ended the
transaction, `scope.mark_terminated()` so teardown is a no-op; otherwise leave
the scope as-is and let the driver CANCEL/BYE. To fully reap an early-CANCEL,
drive the callee's `200`+`487` in-scenario (see `AbandonRinging`).

### Add a smoke test

Add a `#[tokio::test(flavor = "multi_thread")]` to `tests/smoke.rs`: call
`setup(base_port, Correlation::header("X-Loadgen-Id"), sample_cap)` (or
`setup_with(.., |c| …)` to tune the in-process B2BUA, e.g. exhaust the CPS bucket
for an overload test), build a `Driver` over your scenario list, `driver.run()`,
then assert on `reporter.count(id, &class)` and the leak canaries
(`core.registry_size() == 0`, `b2bua.active_calls() == 0`,
`b2bua.assert_fully_reaped()`). Model it on
`loadgen_post_call_cleanup_no_leak` / `loadgen_mux_emergency_split_under_overload`.

### Advanced: multiple receivers on one socket (scenario-owned routing)

The mux correlates a *call* by its token; when two legs of one call land on the
**same** socket, a scenario-supplied `LegPicker` (handed a parsed `LegInfo`)
disambiguates which receiver gets the leg. Declare it via `CallRouting`
(`.leg(addr,label)` per receiver, `.picker(addr, …)`). See
`loadgen_mux_picker_disambiguates_shared_socket` for a worked example. This is
the seam a future multi-REFER / re-route scenario builds on; the mux itself never
reads `X-Api-Call` or any URI — leg routing is the scenario's to own.

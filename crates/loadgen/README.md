# `loadgen` — SIP load generator (a SIPp substitute)

`loadgen` drives **managed-rate SIP load** through a SUT (our front-proxy +
B2BUA, or any SIP element) by **reusing the functional `scenario-harness`
choreography** as load scenarios. One process, thousands of concurrent calls,
bounded memory, a live Prometheus `/metrics` endpoint, and an on-disk callflow
report that keeps the first-N samples per `(scenario × result-class)` — including
the OK flows and, for failures, *why* they failed.

It multiplexes every dialog over a **few static UDP sockets** (one per defined
endpoint: `uac`, `uas`, `refer`), so call rate is not bounded by fds/ephemeral
ports. Calls are correlated by one random per-call token; **how the token
travels through the SUT is a pluggable per-run strategy** (`--correlate`): a
relayed header (default `X-Loadgen-Id`, needs SUT cooperation) or the To-header
user-part (works against any SIP-correct B2BUA). See *Correlation strategies*.

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
`127.0.0.1`), and `--route-pin-to-uas` selects the `api-call-pin` **egress
policy** (the same `EgressPolicy` the e2e framework's layouts declare): the
INVITE carries an `X-Api-Call` destination pin so our B2BUA sends the callee leg
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

- With the default `header` correlation, the B2BUA must be **transparent to the
  correlation header**: deploy it with `B2BUA_RELAY_HEADERS=X-Loadgen-Id`
  (already set in `deploy/k8s/manifests/20-worker.yaml`). Without it the callee
  leg never correlates and you'll see `loadgen_mux_orphan_total` climb / zero OK
  calls — or switch to `--correlate to-user`, which needs no SUT cooperation
  (see *Correlation strategies* below).
- `--bind-ip` = the kind bridge gateway:
  `docker network inspect kind -f '{{(index .IPAM.Config 0).Gateway}}'` (e.g.
  `172.20.0.1`). See the `cluster-nat-inventory` notes for the NAT details.

Key flags: `--cps`, `--duration`, `--max-in-flight`, `--target`, `--bind-ip`,
`--base-port` (uac=base, uas=base+1, refer=base+2), `--correlate` /
`--correlation-header` / `--correlation-template` / `--correlation-extract`
(see *Correlation strategies*), `--route-pin-to-uas`, `--refer-key` (the
`X-Api-Call.refer_key` the SUT's REFER backend authorizes — per-run SUT auth
data fed into the refer scenarios), `--scenario name=weight` (repeatable; omit
for the default mix), `--out-dir`, `--metrics-addr` (default `0.0.0.0:9300`),
`--sample-cap`, `--drop-rate` / `--drop` / `--auto-retransmit` (see *Packet loss
+ auto-retransmit* below). Run `--help` for the full list.

### The environment axis: `--endpoint-config`

Addressing + egress can come from **one authored document** instead of the flags:
`--endpoint-config <file>` loads an `e2e-model` `EndpointConfig` (the same JSON
the ADR-0018 e2e framework binds its Infra shapes with; schema:
`e2e/schemas/endpoint-config.schema.json`). loadgen reads the roles `alice`
(UAC bind), `bob` (UAS bind), `charlie` (REFER-target bind), `lb` (the SUT
ingress), `recvTimeoutMs`, and the optional **`egress` policy**:

```json
{
  "infraShape": "loadgen-mux",
  "roles": {
    "alice":   "172.20.0.1:6000",
    "bob":     "172.20.0.1:6001",
    "charlie": "172.20.0.1:6002",
    "lb":      "172.20.255.250:5060"
  },
  "recvTimeoutMs": 5000,
  "egress": "api-call-pin"
}
```

`egress` is `"transparent"` (default — the SUT routes the callee itself),
`"api-call-pin"` (attach the proprietary `X-Api-Call` destination pin; what
`--route-pin-to-uas` selects), or `{"registrar-aor": {"domain": …}}` (dial the
callee's registered AOR). The flags (`--target`/`--bind-ip`/`--base-port`/
`--route-pin-to-uas`/`--recv-timeout-ms`) stay as **shorthand that synthesizes
an equivalent EndpointConfig** when no file is given. Note: the e2e framework's
compiled infra layouts keep declaring their own policy and override this field;
loadgen standalone is its reader.

### Correlation strategies

Every call mints one random token; the mux demuxes an inbound **initial** leg
(a Call-ID it has never seen) back to its call by recovering that token. A
strategy has two halves — **stamp** (how the token is written into the outgoing
INVITE) and **extract** (how it is recovered from a received leg) — picked
per run with `--correlate`; all mux endpoints share the one strategy:

- **`--correlate header`** (default) — the token rides one transparent header
  the SUT must **relay** onto every leg it originates (our b2bua:
  `B2BUA_RELAY_HEADERS`). Untuned this is byte-for-byte the historic behaviour:
  `X-Loadgen-Id: <token>`, extraction = the whole header value.
  - `--correlation-header <name>` picks the header.
  - `--correlation-template <tpl>` shapes the VALUE around a `${token}`
    placeholder, so the token can ride a **structured** header a third-party
    SUT already relays, e.g.
    `--correlation-header User-to-User --correlation-template '${token};encoding=hex'`
    (RFC 7433 UUI) or
    `--correlation-header P-Charging-Vector --correlation-template 'icid-value=${token}'`
    (PCV).
  - Extraction is a regex whose **first capture group** is the token — derived
    from the template automatically (literal parts escaped, the placeholder
    matched as unreserved URI chars, trailing params tolerated); override it
    with `--correlation-extract '<regex>'` when the SUT rewrites the value
    shape.
- **`--correlate to-user`** — the token IS the To-header **user-part**
  (`To: <sip:lg…@host>`); extraction reads the To user of the arriving INVITE.
  A SIP-correct B2BUA copies the To URI onto its originated leg, so this
  correlates against a **third-party SUT with zero cooperation** (one that
  strips unknown headers breaks `header` correlation entirely). The trade-off:
  the To user is now loadgen-owned, so don't combine it with a SUT that routes
  or rewrites on the To user. (The REFER scenario's `Refer-To` user becomes the
  token too, so the transfer leg keeps correlating.)

Correlation failures are observable either way: an arriving initial INVITE with
no extractable token counts `loadgen_mux_orphan_total{reason="no_header"}`; an
extracted token matching no pending call counts `reason="unknown_token"`.

In code the strategy is `loadgen::Correlation` (`header(name)`,
`header_templated(name, template, extract)`, `to_user()`); the stamp half is
applied inside `CallEnv::outgoing_invite` via `CorrelationStamp` (the per-call
identity half, orthogonal to the egress rewrite), the extract half by the mux
demux. Both strategies are covered by unit tests (`mux::tests`) and the
smoke suite (`loadgen_to_user_correlation_without_relayed_header` proves a full
call correlates with **no** relay configured on the SUT).

### Packet loss + auto-retransmit (robustness testing)

Two default-off knobs let you exercise the SUT (and the loadgen itself) against a
lossy fabric — an un-tuned run is byte-for-byte the historic behaviour.

- **`--drop-rate <f>`** (or **`--drop`** = the `0.001` default, 1/1000 so
  `P(3 drops in a row) ≈ 1e-9`): each datagram this call's mux endpoints send OR
  receive is independently dropped with probability `f`. Dropped datagrams are
  counted in `loadgen_drop_total{dir="out|in"}`. **Without** retransmit a single
  drop fails the call (the harness UAs are scripted, fire-and-forget), so this
  measures how loss-fragile a raw run is.
- **`--auto-retransmit`**: turns on a per-call SIP transaction engine in the mux
  that recovers loss on real timers — **requests** (INVITE Timer A, non-INVITE
  Timer E) retransmitted until answered; **INVITE answers** (2xx) retransmitted
  (Timer G) until the ACK; **our ACK** re-sent on a retransmitted 2xx; **non-INVITE
  answers** re-sent when the peer retransmits the request; and the resulting
  **inbound duplicates absorbed** so the strict scripted `expect` never chokes.
  Retransmits are themselves subject to `--drop-rate`, so recovery is geometric
  (the point of the 1/1000 default). This is the loadgen's own safety net; the SUT
  still does its own retransmission independently.

Both are **per-scenario overridable** inside a `--scenario` spec, after the weight:

```bash
# global 1/1000 loss + recovery, but hammer reinvite at 2/1000 with recovery,
# and leave options_hold lossless:
./target/release/loadgen --target … --bind-ip … --drop --auto-retransmit \
  --scenario basic_call=4 \
  --scenario reinvite=2,drop=0.002,retransmit \
  --scenario options_hold=1,drop=0
```

Note: recovery needs headroom in `--recv-timeout-ms` (default 5000) — a datagram
must be retransmitted and answered inside one recv window, and a two-hop path
(alice→SUT→bob) can need recovery on **both** hops, so keep the timeout wide when
running a heavy loss rate.

**The `18x` ringing provisional is exempt from recovery** — it is a NON-PRACK,
best-effort message (RFC 3261 §13.2.2.4: the dialog/ACK rides the 2xx, a lost 180
does not fail the call). So a dropped `18x` is *expected*, not a failed call: the
caller tolerates its absence and proceeds to the `200`. Instead of failing the
call, the driver tracks the **cross-call delivery rate** and exports it:

- `loadgen_ringing_expected_total` — calls that reached the ring→answer step.
- `loadgen_ringing_received_total` — of those, how many saw their `18x`.

`received / expected` should stay **> 99%** (at the 1/1000 default it is ~99.8%);
a value well below that is a *systemic* 18x regression (a real bug), unlike one
dropped 180. The endurance harness gates on this ratio.

### Realistic timers + the long recorded call

For a run that looks like real traffic (not a tight back-to-back loop), set the
dwell knobs — all default to `0`/off so the smoke suite stays fast:

- `--ring-delay-ms` — the callee dwells between `180` and `200` (e.g. `5000` = a
  5 s ring). Applies to every scenario that establishes.
- `--talk-time-ms` — post-connect talk held before BYE on a basic call.
- `--reinvite-gap-ms` — spacing held **before and after** the re-INVITE.
- `--long-hold-secs` — the hold of the `long_call` scenario (default 1200 = 20 min).

`long_call` is the small **recorded long-tail**: it establishes, fires **exactly
one** in-dialog OPTIONS ping (the marker the recorder captures), then holds the
dialog open for `--long-hold-secs` — answering the SUT's own in-dialog keepalive
OPTIONS on **both** legs so the call is not torn down — then BYEs. Give it a small
weight (≈2 %) in the mix:

```bash
./target/release/loadgen --target … --bind-ip … --route-pin-to-uas \
  --cps 20 \
  --ring-delay-ms 5000 --talk-time-ms 8000 --reinvite-gap-ms 5000 --long-hold-secs 1200 \
  --scenario basic_call=16 --scenario reinvite=4 --scenario long_call=0.4 \
  --background-record-every 1 --sample-cap 50 --report-interval-secs 60
```

`--background-record-every 1` = **full recording** (every call's flow is captured;
stored samples stay bounded by `--sample-cap` per bucket). The cost is per-call
recording memory, so the binary also exports `loadgen_process_resident_memory_bytes`
— watch it (and `loadgen_inflight` / `loadgen_mux_registry_size`) on the Grafana
*Loadgen* dashboard. `--report-interval-secs N` re-writes the on-disk report every
N s so it is browsable mid-run.

### The parameters axis: binding pools (`--case` / `case=`)

The dwell flags above are **global**; identities default to the agent URIs. To
drive **per-call identities and per-call dwells from data**, attach an authored
`e2e-model` **Test case** (the same JSON documents the ADR-0018/0019 framework
runs; schema: `e2e/schemas/test-case.schema.json`) to a mix entry:

```bash
# one case for the whole mix:
./target/release/loadgen --target … --bind-ip … --case e2e/cases/load-basic-pooled.json

# or per mix entry (overrides the global --case for that entry):
./target/release/loadgen --target … --bind-ip … \
  --scenario basic_call=4,case=e2e/cases/load-basic-pooled.json \
  --scenario reinvite=1
```

A case may carry a **binding pool** — `bindings: { mode, entries }` — where each
entry is an `Input` **overlay** (core `from`/`to`/`ruri` + `extras`) merged over
the case's base `input` (entry fields win). Per call the driver resolves ONE
entry — `"mode": "seq"` walks the pool in order, `"random"` picks per seeded
RNG; both **wrap**, so identities repeat once the pool is exhausted (by
design: a finite subscriber pool). String fields (core AND extras) may embed
expansion tokens resolved per call:

- `${seq}` — the monotone per-run call counter;
- `${seq:N}` — the counter zero-padded/truncated to its last `N` digits
  (`7` → `0007`, `123456` → `3456` for `N=4`);
- `${rand:N}` — `N` fresh random digits (deterministic per run seed).

The worked example, `e2e/cases/load-basic-pooled.json`:

```json
{
  "id": "load-basic-pooled",
  "compatibleShapes": ["basic-call"],
  "input": {
    "extras": { "ring_delay_ms": 25, "talk_time_ms": 10 }
  },
  "bindings": {
    "mode": "seq",
    "entries": [
      { "core": { "from": "sip:+3310${seq:4}@pool.example",
                  "to":   "sip:+3390${seq:4}@callee.example" } },
      { "core": { "from": "sip:+4420${rand:6}@pool.example" } }
    ]
  }
}
```

Call 0 dials `From: sip:+33100000@pool.example` → `To: sip:+33900000@…`, call 1
`From: sip:+4420<6 random digits>@…` (falling back to the base/default To), call
2 wraps to entry 0 with `+33100002`, and so on. What the resolution drives:

- the resolved **core `from`/`to`/`ruri`** ride the same egress
  `outgoing_invite` path as an e2e Test case's `core` (folded in before the
  layout's egress rewrite, which keeps the final say — an AOR R-URI or
  `X-Api-Call` pin still wins; a `to-user` correlation stamp overrides an
  authored `to`, since correlation is load-bearing demux infrastructure);
- **recognized extras become per-call dwells**, overriding the global flags
  knob-by-knob: `ring_delay_ms`, `talk_time_ms`, `reinvite_gap_ms`,
  `long_hold_secs`, `options_cadence_ms` (unset knobs keep the global value).
  Unrecognized extras are left alone (they are the open per-shape parameter
  map). This kills the "dwells are global" limitation — `CallConfig` keeps the
  global defaults, the case refines them per call;
- sampled callflow pages show the **resolved binding** in the header banner
  (`binding: case=load-basic-pooled seq=17 entry=1 from=… to=…`), so a stored
  flow says WHICH identity dialed. Prometheus/bucket labels stay
  **scenario-keyed** — a pool never becomes label cardinality.

A malformed token (`${bogus}`, `${seq:}`, an unclosed `${…`) or an empty pool
fails **at startup** (the same load-time validation `validate_case` applies on
the e2e surface), never silently mid-run. Absent `bindings`, the case's single
`input` is used for every call (tokens still expand), and with no `--case` at
all the historic flag-only behaviour is byte-for-byte unchanged. Smoke
coverage: `loadgen_pooled_case_identities_and_dwell_overrides`.

### Test-case checks on sampled calls (`checks` / `checkSets`)

An attached case's **checks** — inline `checks` blocks and referenced
`checkSets` (loaded from `--check-sets-dir`, default `e2e/checksets`, the same
store the e2e runner reads) — are evaluated over a call's recorded trace by
the ONE shared check engine (`e2e_model::checks`). `${input.*}` binds to THIS
call's **resolved (pool-expanded) input**, `${infra.lbVip}` to the run's
`--target`. Any failed check reclassifies an otherwise-OK call to the
**`check_fail`** class (its own Prometheus `class` label + sample directory);
the sampled callflow page lists every verdict, **PASS and FAIL alike**, next to
the flow.

**Honest scope — checks run on SAMPLED calls only.** The unsampled majority
binds no recording (that's what keeps memory flat at load), so there is nothing
to evaluate: checks are a **per-sample oracle, not a per-call gate** — exactly
like the RFC audit. Raise coverage with `--sample-cap` and/or
`--background-record-every 1` (full recording; watch
`loadgen_process_resident_memory_bytes`).

Anchors: check selectors are `<agent>.<anchor>` over the LOAD agent names —
`alice`, `bob`, `bob2` (rerouting), `charlie` (refer) — not the e2e surface's
`bob1`. The shared choreography publishes `initialInvite` /
`firstProvisional` / `answer` / `ack` on establishment (+ `prack` on the
100rel flows), `bye` on hangup, `reInvite` on the reinvite shape, and `refer`
(the REFER bob sends TO the SUT — matched on the sent side, since no test
agent receives it). A shape's published set is declared on its
`ShapeDescriptor` in `e2e-model::registry`. The basic 180 is best-effort, so
key `firstProvisional` from an `optional: true` block unless a lost 18x should
fail the sample.

A case may also carry **`allowViolations`: `["rfc3261.noContactOnBye", …]`** —
the authored analogue of `Harness::allow_violation` for a flow that
legitimately deviates. The named RFC audit rules are exempted per call, so the
finding no longer reclassifies the sampled call to `rfc_audit_fail`. Absent /
empty = today's full audit, byte-for-byte. Smoke coverage:
`loadgen_case_checks_pass_and_render_verdicts`,
`loadgen_failing_check_reclassifies_to_check_fail`,
`loadgen_allow_violations_waives_named_rfc_rule`.

### In the endurance run (parallel to SIPp)

The endurance harness runs `loadgen` as an **in-cluster Job alongside the SIPp
baseline** — same image (`siprustserver:dev`, now carrying the `loadgen` binary),
scraped via the same pod annotations. It is wired in `deploy/k8s`:

- `manifests/45-loadgen-job.yaml` — the Job (binds the mux on the pod IP, pins the
  b-leg back to itself, 20 cps base, the timers above, full recording).
- `endurance.sh` — `LOADGEN_*` knobs (default `LOADGEN_CPS=20`); the stream starts
  in `start_baseline`, is supervised by `ensure_loadgen`, stopped in `stop_streams`,
  and its HTML report is copied out at the end (and anytime via
  `./endurance.sh fetch-loadgen`) into `results/endurance-*/loadgen-report/`.
- `deploy/observability/.../dashboards/loadgen.json` — the Grafana panels
  (completion rate by class/scenario, e2e + checkpoint latency, RSS + leak
  canaries), aligned to the chaos-window annotations.

Disable it for a SIPp-only run with `LOADGEN_ENABLE=0`. REFER is off by default in
the endurance mix (`LOADGEN_W_REFER=0`) because it needs cluster REFER auth; raise
the weight to include it.

---

## Where the results are (and *why* calls failed)

The report is written to `--out-dir`, bucketed per `(scenario × result-class × chaos)`:

- **`index.html`** — counts table (`scenario | class | chaos | count | sample-links`),
  OK rows green, failing rows red; plus latency percentiles and checkpoints.
- **`callflows/<scenario>/<class>/<chaos>/<i>.html`** — the per-call **SIP sequence
  diagram** for sampled calls. For a failing call the page shows `FAIL` **and the
  reason** (the `StepError` / outcome) as the header banner and a `call-result`
  anomaly — e.g. *"alice expected 200, got 486 Busy Here"*,
  *"transfer declined by charlie (603)"*; a sampled NOK also lists the lifecycle
  `[phases: connected@…ms, reinvited@…ms]` it reached. The failure `<class>` is a
  directory name: `status_503`, `status_486`, `timeout`, `unexpected`,
  `rfc_audit_fail`, `check_fail`, `panic`, `transport`, `unparseable`. A call
  with an attached Test case also lists its check verdicts (PASS and FAIL).
- **`summary.md`** — the same counts in markdown.
- **Live:** `curl <metrics-addr>/metrics` during a run for the per-`(scenario,
  class, chaos)` counters plus the `loadgen_mux_orphan_total` /
  `loadgen_mux_registry_size` canaries and `loadgen_chaos_markers_total`.

### Chaos correlation (near vs clear)

When a fault is injected during a run, the chaos driver flags the loadgen at the
kill instant via **`POST /chaos?type=<kind>&target=<who>`** (same socket as
`/metrics`; the endurance harness does this in `loadgen_chaos_flag`). Each
finished call is then auto-classified on the `chaos` label:

- **`chaos="near"`** — an injected fault landed on a *fragile moment* of the
  call: within `--chaos-phase-tolerance-ms` (default 200) of a dialog-state
  transition (connected/reinvited/transferred/…), or mid-setup before it
  connected. The state had no time to propagate and SIP retransmission normally
  recovers it → likely acceptable kill collateral (in-setup at the kill → 408,
  etc.) — **counted, but not hand-triaged.**
- **`chaos="clear"`** — the call was stably connected across the fault (or none
  overlapped). A genuine SUT signal.

The loadgen timestamps the marker on its **own** clock — the single process-wide
`Clock::system()` the calls also record on — so the overlap is exact (no
Call-ID/tag ms-base reconciliation) and the marker renders on the very axis the
frames do, even if the host wall clock steps mid-run. A post-reboot call (created
seconds after the worker came back) is `clear`, so a reclaim-path defect stays
visible. **Triage `loadgen_calls_total{class="rfc_audit_fail",chaos="clear"}`**
and the `callflows/<sc>/<class>/clear/` flows; revisit `near` only if wanted.

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
     `env.charlie`), `env.via` (the SUT ingress) and `env.outgoing_invite(&["bob"],
     inv)` — the egress seam that realizes the logical INVITE on this run's wire
     (correlation stamp + emergency marker + the run's `EgressPolicy` rewrite,
     mirroring e2e-core `InfraRuntime::outgoing_invite`) — plus `env.callee(role)`
     to resolve any callee target and the REFER helpers (`env.refer_to()`,
     `env.refer_authorization(refer_key)`). Per-run SUT auth data (the refer
     scenarios' `refer_key`) is fed at CONSTRUCTION via `ScenarioInputs`, not the
     env.
   - **Register your dialog state in `scope`** as the call progresses
     (`set_early` once the INVITE is out, `set_confirmed` once it answers,
     `mark_terminated` once you tear it down) so a mid-flow failure is still
     cleaned up by the driver — this is what keeps the SUT leak-free.
   - `ctx.checkpoint("name")` records a latency checkpoint (shows in the report).

2. Declare it ONCE in the unified, open shape registry
   (`e2e_model::registry::default_shapes`, or `ShapeRegistry::register` from a
   third-party crate): a `ShapeDescriptor::new("my_flow")` carrying the load
   attributes (`.needs_charlie()` / `.needs_bob2()` / `.emergency()`), an
   optional `.default_weight(w)` for the default mix, and the body factory —
   `.load_shared(Arc::new(MyFlow))`, or `.load_with(|inputs| …)` when it needs
   per-run SUT auth data (`ScenarioInputs`). The driver resolves it via
   `MixEntry::by_id` / `--scenario my_flow=…`. An **emergency variant** is
   free: a second descriptor (`"my_flow_em"`) with `.emergency()` reusing the
   same body — the report id comes from the descriptor. A shape that ALSO has
   an e2e functional body attaches it by the same id in
   `e2e-core/src/shapes/mod.rs::default_bodies` (see `rerouting_prack`, the
   first dual-body shape).

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
for an overload test; `setup_no_relay(..)` for the third-party-SUT shape with no
header relay), build a `Driver` over your scenario list, `driver.run()`,
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

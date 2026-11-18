# `loadgen`: SIP load generator (a SIPp substitute)

`loadgen` drives **managed-rate SIP load** through a SUT (the front proxy and
B2BUA, or any SIP element) by reusing the functional `scenario-harness`
choreography as load scenarios. One process, thousands of concurrent calls,
bounded memory, a live Prometheus `/metrics` endpoint and report page on the
same socket, and an on-disk callflow report that keeps the first N samples per
`(scenario × result-class)`, OK flows included, and for a failure the reason.

It multiplexes every dialog over a **few static UDP sockets** (one per endpoint
address: `uac`, `uas`, `refer`, or one with `--shared-socket`), so the call rate
is not bounded by fds or ephemeral ports. Calls are correlated by one per-call
token; `--correlate` picks how the token travels through the SUT (see
[Correlation strategies](#correlation-strategies)). `loadgen --help` lists every
flag.

---

## Quick start

### 1. In-process (no cluster): the smoke suite

The smoke tests stand up an in-process `B2buaCore` SUT on real loopback UDP and
drive the load driver against it:

```bash
cargo test -p loadgen --test it smoke::
cargo test -p loadgen --test it smoke::loadgen_post_call_cleanup_no_leak -- --nocapture
```

Most carry `#[ignore = "slow lane: loadgen"]`: `just test-slow 'package(=loadgen)'`
runs them, and `-- --include-ignored` after a `cargo test` above runs them with
the others.

### 2. Host to a real cluster: the `loadgen` binary

Point it at the front proxy's external VIP. The mux endpoints bind on a host IP
**the proxy reaches on the callers' plane** (the external bridge's gateway, not
`127.0.0.1`). `--route-pin-to-uas` selects the `api-call-pin` egress policy: the
INVITE carries an `X-Api-Call` destination pin so the B2BUA sends the callee leg
back to the host `uas` socket.

```bash
cargo build --release -p loadgen

./target/release/loadgen \
  --target 192.168.60.250:5060 \      # front proxy's external VIP
  --bind-ip 192.168.60.1 \            # the host's address on the callers' bridge
  --route-pin-to-uas \
  --scenario basic_call=4 --scenario reinvite=2 --scenario refer=1 \
  --cps 50 --duration 60 \
  --out-dir ./loadgen-report
```

- With the default `header` correlation the B2BUA must relay the correlation
  header: `B2BUA_RELAY_HEADERS=X-Loadgen-Id` (set in
  `deploy/helm/siprust/values-kind.yaml`). Without it the callee leg never
  correlates and `loadgen_mux_orphan_total` climbs; `--correlate to-user` needs
  no SUT cooperation.
- `--bind-ip` is the external bridge's gateway:
  `docker network inspect sipext -f '{{(index .IPAM.Config 0).Gateway}}'`.
- `--base-port` lays out uac=base, uas=base+1, refer=base+2.
- Each mux socket requests 2 MiB receive and send buffers, since one socket
  carries every concurrent call. loadgen warns at bind when `net.core.rmem_max`
  or `wmem_max` clamped the request.

### The run-spec axis: `--load-profile`

The whole run can come from one `LoadProfile` JSON
(`e2e/schemas/load-profile.schema.json` lists its keys): the run knobs, the
global `robustness` defaults, `overloadReject`, and the scenario `mix` (each
entry: `shape` id, `weight`, an optional `case`, per-entry `dropRate` /
`retransmit`). `e2e/loadprofiles/endurance-baseline.json` is an example.

```bash
./target/release/loadgen \
  --target 192.168.60.250:5060 --bind-ip 192.168.60.1 --route-pin-to-uas \
  --load-profile e2e/loadprofiles/endurance-baseline.json
```

**A flag overrides the profile.** The profile supplies defaults; an explicit
CLI flag wins for its knob, and an omitted field falls back to the flag's
default. An explicit `--scenario` (or `--case`) replaces the whole `mix`. An
`--endpoint-config` file's `recvTimeoutMs` wins over the profile's. A malformed
profile fails at startup.

### Live rate control (`POST /rate`)

The offered rate changes at runtime over the `/metrics` socket:

- `POST /rate?cps=<float>` sets the target, clamped to `>= 0`; a missing or
  unparseable `cps` is a `400`. The response echoes the applied value.
- `GET /rate` returns the current target, also the `loadgen_target_cps` gauge.
- `cps=0` pauses new-call admission (in-flight calls complete); a later
  `POST /rate?cps=N` resumes.
- `POST /drain` stops admission and lets the run finish.
- A rate change fires no catch-up burst; a raise takes effect within one slot.

```bash
curl -X POST 'http://<metrics-addr>/rate?cps=40'
curl -X POST 'http://<metrics-addr>/rate?cps=0'
curl       'http://<metrics-addr>/rate'
```

### The environment axis: `--endpoint-config`

Addressing and egress can come from one `e2e-model` `EndpointConfig`
(`e2e/schemas/endpoint-config.schema.json`). loadgen reads the roles `alice`
(UAC bind), `bob` (UAS bind), `charlie` (REFER-target bind), `lb` (the SUT
ingress), `recvTimeoutMs`, and the optional `egress` policy:

```json
{
  "infraShape": "loadgen-mux",
  "roles": {
    "alice":   "192.168.60.1:6000",
    "bob":     "192.168.60.1:6001",
    "charlie": "192.168.60.1:6002",
    "lb":      "192.168.60.250:5060"
  },
  "recvTimeoutMs": 5000,
  "egress": "api-call-pin"
}
```

`egress` is `"transparent"` (default: the SUT routes the callee itself),
`"api-call-pin"` (what `--route-pin-to-uas` selects), or
`{"registrar-aor": {"domain": …}}` (dial the callee's registered AOR). Without
a file, the `--target` / `--bind-ip` / `--base-port` / `--route-pin-to-uas` /
`--recv-timeout-ms` flags build the same config.

### Correlation strategies

Every call carries one token; the mux demuxes an inbound initial leg (a
Call-ID it has never seen) back to its call by recovering that token. A
strategy has a stamp half (how the token is written into the outgoing INVITE)
and an extract half (how it is read off a received leg):

- **`--correlate header`** (default): the token rides one header the SUT
  relays onto every leg it originates, `X-Loadgen-Id: <token>` untuned.
  `--correlation-header` picks the header; `--correlation-template` shapes the
  value around `${token}`, so the token can ride a structured header the SUT
  already relays, e.g.
  `--correlation-header User-to-User --correlation-template '${token};encoding=hex'`
  (RFC 7433). Extraction is a regex whose first capture group is the token,
  derived from the template; `--correlation-extract` overrides it.
- **`--correlate to-user`**: the token is the To-header user part. A
  SIP-correct B2BUA copies the To URI onto its originated leg, so this works
  against a third-party SUT with no cooperation. Do not combine it with a SUT
  that routes or rewrites on the To user.
- **`--correlate from-user`**: the token is the caller's From user, taken from
  the attached Test case's resolved `core.from`; nothing is stamped. Every mix
  entry must carry a case (checked at startup). A number a concurrent call
  holds, or one cooling for 64·T1 after a call that ended not ok, is re-drawn
  from the pool, up to 8 draws, then the call is `rejected`.

| Counter | Counts |
|---|---|
| `loadgen_mux_orphan_total{reason="no_header"}` | an initial INVITE with no extractable token |
| `loadgen_mux_orphan_total{reason="unknown_token"}` | a token matching no pending call |
| `loadgen_mux_orphan_total{reason="released"}` | an INVITE on a Call-ID a finished call released within 64·T1 |
| `loadgen_mux_orphan_total{reason="early"}` | a leg arriving before its caller sent the INVITE |
| `loadgen_mux_orphan_total{reason="call_id_reuse"}` | a new INVITE reusing a caller's own Call-ID (RFC 3261 §8.1.1.4) |
| `loadgen_mux_orphan_total{reason="stray"}` | a request with no call and no held answer |
| `loadgen_mux_late_retransmission_answered_total` | a retransmission after the call ended, answered again with the held final (RFC 3261 §17.2.2, Timer J) |
| `loadgen_mux_token_collision_total`, `loadgen_mux_key_cooling_total` | `from-user` draws refused (draws, not calls) |
| `loadgen_mux_caller_key_mismatch_total` | a caller INVITE whose From user is not its key |

### Shared socket

`--shared-socket` binds `alice`, `bob` and `charlie` on one address
(`--base-port`); an `--endpoint-config` giving the roles one address is the
explicit form. It fits a SUT that sends each new leg back to the ip:port that
originated the call. Every callee leg of a call shares the `uas` socket in any
layout: the token picks the call, then a `LegPicker` on the R-URI user prefix
picks the leg (`bob`, `bob2`, `charlie`; longest match wins).

### Packet loss and auto-retransmit

- **`--drop-rate <f>`** (or `--drop` = `0.001`): each datagram the call's mux
  endpoints send or receive is dropped with probability `f`, counted in
  `loadgen_drop_total{dir="out|in"}`. Without retransmit one drop fails the
  call.
- **`--auto-retransmit`**: a per-call transaction engine in the mux recovers
  loss on the RFC 3261 timers (A, E, G) and absorbs the inbound duplicates.
  The SUT still retransmits on its own.

Both are overridable per scenario, after the weight:

```bash
./target/release/loadgen --target … --bind-ip … --drop --auto-retransmit \
  --scenario basic_call=4 \
  --scenario reinvite=2,drop=0.002,retransmit \
  --scenario options_hold=1,drop=0
```

Keep `--recv-timeout-ms` (default 5000) wide under heavy loss: a two-hop path
can need recovery on both hops inside one receive window.

The `18x` ringing provisional is exempt from recovery (non-PRACK, best effort,
RFC 3261 §13.2.2.4), so a lost 18x does not fail the call. The driver exports
`loadgen_ringing_expected_total` and `loadgen_ringing_received_total`; their
ratio stays above 99 % at the default loss rate, and a value well below is an
18x regression.

### Realistic timers and the long recorded call

The dwell knobs default to `0`: `--ring-delay-ms` (between 180 and 200),
`--talk-time-ms` (before BYE), `--reinvite-gap-ms` (before and after the
re-INVITE), `--long-hold-secs` (the `long_call` hold, default 1200).
`long_call` establishes, sends one in-dialog OPTIONS, answers the SUT's
keepalives on both legs for `--long-hold-secs`, then BYEs.

```bash
./target/release/loadgen --target … --bind-ip … --route-pin-to-uas \
  --cps 20 \
  --ring-delay-ms 5000 --talk-time-ms 8000 --reinvite-gap-ms 5000 --long-hold-secs 1200 \
  --scenario basic_call=16 --scenario reinvite=4 --scenario long_call=0.4 \
  --background-record-every 1 --sample-cap 50 --report-interval-secs 60
```

`--background-record-every 1` records every call (stored samples stay bounded
by `--sample-cap` per bucket); watch `loadgen_process_resident_memory_bytes`.
`--report-interval-secs N` rewrites the on-disk report every N s.

### The parameters axis: binding pools (`--case` / `case=`)

An `e2e-model` Test case (`e2e/schemas/test-case.schema.json`) attached to the
whole run (`--case`) or to one mix entry (`--scenario basic_call=4,case=…`)
drives per-call identities and dwells from data. Its `bindings: { mode,
entries }` pool holds `Input` overlays merged over the case's base `input`;
`"seq"` walks the pool in order, `"random"` picks by the run's seeded RNG, and
both wrap. String fields may embed tokens resolved per call:

- `${seq}`: the per-run call counter;
- `${seq:N}`: the counter zero-padded or truncated to its last `N` digits;
- `${rand:N}`: `N` random digits, deterministic per run seed.

`e2e/cases/load-basic-pooled.json` is the worked example. The resolved core
`from` / `to` / `ruri` ride the egress path (the egress rewrite and a
`to-user` stamp keep the final say). The extras `ring_delay_ms`,
`talk_time_ms`, `reinvite_gap_ms`, `long_hold_secs` and `options_cadence_ms`
override the global dwell flags for that call. A sampled page shows the
resolved binding; metric labels stay scenario-keyed. A malformed token or an
empty pool fails at startup.

### Test-case checks on sampled calls

An attached case's `checks` and `checkSets` (from `--check-sets-dir`, default
`e2e/checksets`) run over a call's recorded trace through `e2e_model::checks`.
`${input.*}` binds to the call's resolved input, `${infra.lbVip}` to
`--target`. A failed check reclassifies an OK call to `check_fail`.
`allowViolations` exempts named RFC audit rules per call.

**Checks run on sampled calls only**, like the RFC audit: an unsampled call
records nothing. Raise coverage with `--sample-cap` or
`--background-record-every 1`. Check selectors are `<agent>.<anchor>` over the
load agents (`alice`, `bob`, `bob2`, `charlie`); a shape's anchors are declared
on its `ShapeDescriptor` in `e2e-model::registry`. Key `firstProvisional` from
an `optional: true` block, since the 18x is best effort.

### In a soak

A run profile of `tooling/hakit` (ADR-0049) runs `loadgen` as a stream
(`driver: loadgen`), as a container on the external plane or a pod
(`placement: pod`). The runner starts, drains (SIGUSR1) and stops it, reads its
counters and call-outcome records, and attributes its failures to the incident
windows. `tooling/hakit/content/dashboards/loadgen.json` holds its Grafana
panels.

---

## Where the results are

The report is written to `--out-dir`, bucketed per
`(scenario × result-class × case)`:

- **`index.html`**: the command line, the headline figures, the time series,
  the last failures, the counts, latency, round-trip and check tables, the rate
  changes and the settings. It is rendered from `load-result.json` alone
  (`loadrun-render`).
- **`callflows/<scenario>/<class>[/<case>]/<i>.html`**: the SIP sequence
  diagram of a sampled call. A failing call shows `FAIL` and the reason (e.g.
  *"alice expected 200, got 486 Busy Here"*). The class is `status_<code>`,
  `timeout`, `unexpected`, `rfc_audit_fail`, `check_fail`, `panic`,
  `transport` or `unparseable`; a failed call's case bucket is the step it
  failed at, `<who>[:<goal kind>#<goal index>]@<barrier>`.
- **Live report**: `http://<metrics-addr>/` (printed at startup) renders the
  same page from the running reporter, with the rate and drain controls.
- **stderr**: a status line every 5 s.
- **Metrics**: `<metrics-addr>/metrics`, every series labelled with `--stream`
  first: per-`(stream, shape, class)` counters, `loadgen_branch_total` (ok
  calls by accepted ending), `loadgen_failed_step_total`, the latency
  histograms, `loadgen_rtt_seconds{stream,shape,exchange}`, and the
  `loadgen_mux_orphan_total` / `loadgen_mux_registry_size` canaries.

Sampling is bounded: a call records when it is among the first `--sample-cap`
of its scenario or in its scenario's fixed share of 1 in
`--background-record-every`. An error burst thus never raises the recording
cost. A non-sampled failure still gets a stub page with its reason.

### Calls across an incident

loadgen does not judge whether a failure is an incident's collateral: each
call's record carries its own instants, and the HA tooling attributes it to the
incident window it fell in (ADR-0049 §3.5). With `--auto-retransmit`, a call
whose in-dialog request (not a BYE) still awaits its final when the call ends
is held until the final or Timer F (64·T1); the request then fails stamped at
the call's end. A held call does not count against `--max-in-flight` and
delays the drain by up to 32 s.

### Call-outcome records

Every placed call writes one `call-outcome.v1` record
(`tooling/hakit/schemas/call-outcome.v1.schema.json`, ADR-0049 §4): what the
caller saw on the wire and how each callee leg ended; the result class rides in
`producer_verdict` only. Records go to `--records-file` (default
`<out-dir>/call-outcomes.jsonl`) and, with `--records-url`, to VictoriaLogs'
`/insert/jsonline`. A call never placed writes no record and counts in
`loadgen_not_placed_total{stream,shape,reason}`. `--list-shapes` prints the
registered shape ids.

---

## How it relates to the test suites

A load scenario drives a full call with the fallible (`try_*`) agent methods
the functional tests use, so an expected failure is a counted `StepError`,
never a panic. `crates/loadgen/tests/it/smoke.rs` is the regression gate:
correlation, no dialog mixing, no leak, orphans, the picker, the overload
split and post-call cleanup. Load scenarios tolerate reordering on purpose;
strict per-message RFC oracles stay in `b2bua-harness`.

## How to add a scenario

1. Write the load body, an `ActorScenario` (`scenario_harness::actor`). The
   usual body is a `ShapePlan` from `callshapes`: an establishment, stages on
   the current dialog, a teardown, routing through a `RouteBinder`. The shipped
   shapes are in `crates/callshapes/src/shapes.rs`.
2. Declare it once in the shape registry (`e2e_model::registry::default_shapes`,
   or `ShapeRegistry::register` from another crate): a
   `ShapeDescriptor::new("my_flow")` with its load attributes
   (`.needs_charlie()`, `.needs_bob2()`, `.emergency()`), an optional
   `.default_weight(w)` or `.abnormal_end_weight(w)`, its `.anchors(…)`, and the
   body factory. A duplicate id panics at registration. An emergency variant is
   a second descriptor reusing the same body.
3. Add a smoke test in `tests/it/smoke.rs` on `setup(..)` (or `setup_with`,
   `setup_no_relay`), modelled on `loadgen_post_call_cleanup_no_leak`, asserting
   the counts and the leak canaries.

### Accepted endings

A scenario declares the endings it accepts as named branches
(`scenario_harness::actor::{Expect, Branch}`): `answered`, `rejected`,
`early_media_rejected`, `abandoned`, `answered_despite_cancel`,
`answered_before_cancel`, `transfer_declined`, or a scenario-named predicate. A
call that settles on one classes `ok` and counts under its branch; it still runs
the RFC audit and the checks. Every other ending fails under its cause's class.

How a SUT refuses a call under overload is its own convention, so a run that
drives overload declares that final (`--overload-reject 503`, or
`"overloadReject": 503`). That final to a non-emergency INVITE before it
connects is then the `refused_overload` branch; unset, it fails. A scenario
opts out with `Expect::without_overload_refusal()`.

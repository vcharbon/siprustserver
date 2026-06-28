# loadgen multiplexed transport — design note (final, for build)

## Why

The current loadgen binds **one ephemeral UDP socket per call leg** and pins the
callee leg to that socket's *dynamic* address. That hits an OS ceiling
(~6–9 k fds at 100 cps × 30 s; default `ulimit -n` 1024; ephemeral-port + kernel
socket-memory limits) and depends on *dynamic* per-call routing the SUT must
honor. A SIPp replacement must instead multiplex many dialogs over a **few static
sockets**, and must work against **any** SIP element — not just our B2BUA.

## Decisions (locked)

- **Mux only.** The ephemeral transport is replaced (not kept as a mode).
- **One socket per *defined endpoint*** — not a sharded pool. A "defined endpoint"
  is a logical SIP identity (`uac`, `uas`, `refer-target`, …) = exactly one UDP
  socket, multiplexing all of that endpoint's dialogs by dialog identity. An
  endpoint may act as **caller, called, or both** on its single socket.
- **SUT-agnostic correlation** via a **random UUIDv4** in a **generic** header
  (`X-Loadgen-Id`). The only SUT obligation is **transparency** to that header.
  Demux falls back to **Call-ID** (a proxy preserves it → works with zero SUT
  cooperation); the UUID header is needed only when a B2BUA mints a fresh callee
  Call-ID.
- **Recording layered *above* the UDP/demux layer, per call, free when off** (see
  "Recording" — it reuses the existing `AgentBinder` record flag).
- **No leak, loud on the unexpected** (see "No-leak" / "Observability").

## Topology

```
        X-Loadgen-Id: <uuid>                 X-Loadgen-Id: <uuid>  (SUT transparent)
[uac endpoint] ── INVITE ──►  [ any SIP SUT ]  ── INVITE ──►  [uas endpoint]
  (1 socket, all UAC dialogs)                   (1 socket, all UAS dialogs;
                                                 new Call-ID if SUT is a B2BUA)
```

Both endpoints live in the load tool. The SUT routes the callee leg to the `uas`
endpoint's **static** address (its own dialplan / config; for our b2bua an
optional `X-Api-Call destination` adapter supplies that static address). Routing
is static — only the UUID varies.

## Layering (the recording constraint)

```
   Agent / SIP scenario  (unchanged — drives `Arc<dyn UdpEndpoint>`)
        ▲
   ┌────┴───────────────┐  recorded call only (sampled):
   │ recording fake layer│  with_all_contracts(mux_net) → tees send/recv to Recorder
   └────┬───────────────┘  → existing projection/render_html, verbatim
        ▲                   (ABSENT for non-recorded calls — zero CPU)
   MuxNetwork (per call)  : SignalingNetwork; bind_udp → MuxEndpoint
        ▲
   MuxCore (process-wide) : the real UDP sockets + per-socket dispatcher + registry
```

The mux is a **`SignalingNetwork`**. So the *existing*
`AgentBinder::with_network(mux_net, clock, kind, recv_timeout, record)` already
does the right thing:

- `record = false` → `Agent.ep` is the bare `MuxEndpoint` (real UDP + demux,
  direct to the SIP layer, **no recording overhead**).
- `record = true` → `with_all_contracts` wraps the same `MuxEndpoint` with the
  existing recording layer → the on-disk callflow report works unchanged.

**No new recording code; no `AgentBinder` change** — pass a `MuxNetwork` as the
network. This is the whole point of having recording sit on the `SignalingNetwork`
seam.

## Components (`crates/loadgen/src/mux.rs`)

```
MuxCore (Arc, process-wide)
 ├─ endpoints: Map<name → MuxSocket>   // one real UdpSocket each (uac, uas, refer)
 │    each MuxSocket: tokio UdpSocket + a dispatcher recv-loop
 └─ registry: Registry                 // shared, sharded-by-hash Mutex<HashMap> (no dashmap)

Registry
 ├─ by_call_id: Mutex<HashMap<String, Inbox>>   // known dialogs (UAC + promoted UAS)
 ├─ by_uuid:    Mutex<HashMap<Uuid, Pending>>   // pending UAS legs (uuid, inbox, deadline)
 └─ counters: orphans / pending / size

MuxNetwork { core: Arc<MuxCore>, uuid }   // PER CALL; impl SignalingNetwork
 └─ bind_udp(addr) → MuxEndpoint for (this call, role-by-addr)

MuxEndpoint : UdpEndpoint                 // what the Agent (or recording wrapper) holds
 ├─ inbox: bounded mpsc::Receiver<UdpPacket>
 ├─ socket: Arc<UdpSocket>                // the endpoint's shared send socket
 ├─ role + keys registered               // for deregister on Drop
 └─ recv()=inbox.recv; try_recv()=inbox.try_recv;
    send_to()=socket.send_to + (UAC) sniff own Call-ID once → register by_call_id;
    local_addr()=endpoint socket addr; Drop=deregister all keys
```

### Dispatcher (one recv-loop per endpoint socket)

Per inbound datagram, parse Call-ID (+ `X-Loadgen-Id` only if Call-ID unknown):
1. Call-ID in `by_call_id` → push to that inbox.
2. else request with `X-Loadgen-Id` in `by_uuid` → push to that UAS inbox, then
   **promote**: `by_call_id[new_call_id]=inbox`, remove the `by_uuid` entry.
3. else → **orphan**: `loadgen_mux_orphan_total{reason}` (`no_header` /
   `unknown_uuid` / `unknown_callid`) + bounded first-N sample → **drop** (never
   queued).

## Correlation (per call)

- Mint a random UUIDv4 (`uuid` crate, already in lock via getrandom).
- UAC scenario stamps `X-Loadgen-Id: <uuid>` on the INVITE (`Invite::with_header`).
- UAC endpoint self-registers its (harness-minted) Call-ID on first `send_to`.
- UAS endpoint registers `by_uuid[uuid]` at bind; the callee INVITE (unknown
  Call-ID, carrying the uuid) is matched and promoted to its Call-ID.
- A **proxy** SUT preserves Call-ID, so even with no header the callee leg is the
  same dialog — case 1 alone correlates it.

## No-leak guarantees

- **Per-call reclaim.** `MuxEndpoint::Drop` removes its `by_call_id` / `by_uuid`
  entries; the driver drops every call's endpoints at call end (success / failure /
  panic — inside the existing `catch_unwind` + teardown). Registry size tracks
  **in-flight calls only**.
- **Stale-pending reaper.** Each `by_uuid` entry has a deadline (≈ `recv_timeout`).
  Lazy eviction on insert + a coarse periodic sweep removes callee legs that never
  arrived → `loadgen_mux_pending_expired_total`.
- **Bounded inboxes.** Bounded `mpsc`; overflow → drop + `loadgen_mux_inbox_drop_total`.
- **Orphans dropped, not stored** (≤ N sampled).

## Observability ("notify if new dialog without / with-non-matching header")

Prometheus + final report:
- `loadgen_mux_orphan_total{reason=no_header|unknown_uuid|unknown_callid}` + bounded
  orphan samples (offending first-line / Call-ID / From) in the report.
- `loadgen_mux_registry_size` (leak canary — should ≈ in-flight calls).
- `loadgen_mux_pending` / `loadgen_mux_pending_expired_total`.
- `loadgen_mux_inbox_drop_total`.

## CPU note

One dispatcher task parses every inbound datagram on its endpoint socket (Call-ID,
sometimes a header). At extreme pps a single dispatcher can saturate one core;
SO_REUSEPORT sharding behind the same endpoint address can be added later
transparently (the registry is already shared). Out of scope now per the
"one socket per endpoint" decision.

## Code touch points

- **NEW** `crates/loadgen/src/mux.rs` — `MuxCore`, `MuxNetwork` (`SignalingNetwork`),
  `MuxEndpoint` (`UdpEndpoint`), dispatcher, registry, reaper, counters.
- **loadgen ctx/scenarios** — `CallEnv` carries the per-call `uuid` + header name;
  scenarios stamp `X-Loadgen-Id` on the INVITE. `X-Api-Call destination` becomes the
  optional our-b2bua routing adapter only.
- **loadgen driver** — replace `AddrPlan` with a `MuxCore`-backed `BinderFactory`
  (`AgentBinder::with_network(MuxNetwork(core, uuid), …, record)`); mint uuid per
  call; reporter gains the mux series. Remove the ephemeral path.
- **loadgen report** — mux counters + orphan samples.
- **deps** — add `uuid = { version = "1", features = ["v4"] }`. No `dashmap`.
- **scenario-harness** — **none** (mux is a `SignalingNetwork`; the existing
  `with_network` record flag does the recording layering).
- **smoke test** — point the b2bua's b-leg at the static `uas` endpoint address
  (`route_all_to(uas_addr)`); assert mux correlation, no orphan/leak, OK callflow.

## Build order

1. `MuxCore` + `MuxEndpoint` + `MuxNetwork` + dispatcher + registry (+ counters).
2. Wire driver to mux; mint uuid; stamp header; remove ephemeral `AddrPlan`.
3. Reaper + orphan observability + reporter series.
4. Update smoke tests (correlation, no-leak/orphan, OK callflow via recording).
5. (deferred) real cluster.

---

## As-built addenda (2026-06-28)

The locked design above stands; these refine it from the build.

### Correlation: header-ONLY, two layers (call vs leg)

The "Call-ID fallback / multi-source" framing is replaced by a clean split:

- **Call correlation = ONE per-call token** in the transparent header
  (`X-Loadgen-Id`). It answers only *"which call is this?"*. The To-/Request-URI
  hijacking is **removed** (it breaks against any SUT that routes on those URIs).
  Because a relaying SUT copies the header onto *every* originated leg, a call's
  bob and charlie legs **share one token** (the old two-token scheme is gone).
- **Leg routing = scenario-owned.** The mux demuxes purely by `(socket, token)`
  and reads **nothing else** — never `X-Api-Call` (that header is the scenario's
  way of driving our fake-B2BUA backend, not a mux input). When >1 receiver
  shares a socket, the mux calls a **scenario-supplied `LegPicker`**, handed a
  parsed `LegInfo` (R-URI, headers, …), to pick the receiver. A single-receiver
  socket never calls it.

The token entry is **non-consuming** (persists for the call, freed on agent
`Drop`/reaper) and leg-spawn is gated on an initial INVITE, so re-routing /
multi-REFER / re-REFER (further legs of one call on a socket) each promote their
own dialog. `CallRouting{token, legs:[(addr,label)], pickers}` is the per-call
declaration the driver builds before binding.

### SUT transparency (the only SUT obligation)

Implemented as an opt-in B2BUA header relay: `B2buaConfig.relay_headers`
(`B2BUA_RELAY_HEADERS=X-Loadgen-Id`), copied in `build_b_leg` — the single mint
point both the callee leg (bob) and the REFER transfer leg (charlie) flow
through, so the token reaches both. Default empty = no production change.

### Emergency / overload split

`Resource-Priority: esnet.0` marks an emergency call (force-admitted; never
shed). `establish` is **shed-aware**: it races bob's inbound INVITE against
alice's response, so an overload 503 surfaces as `WrongStatus{503}` →
`status_503` (and marks the scope terminated — a final ended the txn, so no
spurious CANCEL). `AsEmergency` wraps any scenario under a distinct id. Report
shows the OK vs 503 split with first-N samples per class.

### Voluntarily-failing scenarios (post-call-cleanup coverage, no endurance)

`failures.rs`: `InviteReject` (486 final → no teardown), `AbandonRinging`
(quit on 180 → CANCEL an early dialog), `ReferCharlieReject` (603 → BYE a
still-confirmed call whose transfer leg was rejected). With `FailMidCall`
(confirmed→BYE) they cover the teardown matrix; `loadgen_post_call_cleanup_no_leak`
asserts the SUT fully reaps (no live call / lock / stamp) after the mix. This
caught a real B2BUA leak: the Tier-3 overload-shed `return`ed under the held
per-call lock without the orphan-teardown, stranding a `locks`-map entry.

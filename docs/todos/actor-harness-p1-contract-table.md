# Actor-harness P1 downstream-contract table

**P1-entry artifact** for plan §4.2 (B7, decisions 3/5) of
[`actor-harness-impl-plan.md`](actor-harness-impl-plan.md). Read that section
(lines ~238–303) alongside this.

This table pins — **before the actor port is written** — every downstream
contract the actor runner must reproduce byte-for-byte: the `StepError` a body
returns on its NOK terminal, the phase trail it stamps, the message anchors it
publishes, its checkpoints, and where it feeds the cross-call 18x gate. These
strings are load-report keys (Prometheus `class` labels + sample-directory
segments + anchor selectors the e2e check engine resolves), so a drift is a
silent contract break, not a cosmetic one.

Everything below is cited `file:line` against the tree at the time of writing;
discrepancies vs the plan's assumed line numbers are collected in
[§8 Verification notes](#8-verification-notes).

> **The cardinality rule (load-bearing).** A step's `who` string keys the sample
> bucket, so it MUST stay bounded-cardinality: a role name (`alice`, `bob`,
> `bob2`, `charlie`) or a fixed synthetic label (`refer`, `rerouting_prack`,
> `alice-abandoned-after-ringing`). Free-form text — the `detail` on
> `UnexpectedKind`/`Unparseable`/`Transport`, panic messages, RFC finding
> details — is **NEVER** keyed (it embeds Call-IDs/branches). `class.rs` keys on
> the `StepError` *variant* + `who` + last phase only; the `Display` text is for
> humans (`crates/scenario-harness/src/agent.rs:88-90`,
> `crates/loadgen/src/class.rs:154-161`). The actor port's `into_result` must
> mint the SAME `who` strings — never a new free-form one.

---

## 1. How `class.rs` derives the case key

Two levels, both in `crates/loadgen/src/class.rs`:

1. **`ResultClass`** (the low-cardinality `class` label + top-level sample dir)
   is derived from the `StepError` **variant** —
   `From<&CallOutcome> for ResultClass`, `class.rs:108-125`:
   - `Timeout | QueueClosed` → `ResultClass::Timeout` → label `"timeout"`
     (`class.rs:116`, `class.rs:59`)
   - `WrongStatus { got, .. }` → `ResultClass::WrongStatus(got)` → label
     `"status_{got}"` (`class.rs:117`, `class.rs:61`)
   - `WrongMethod` → `"wrong_method"` (`class.rs:118`)
   - `UnexpectedKind` → `ResultClass::Unexpected` → label `"unexpected"`
     (`class.rs:119`, `class.rs:63`)
   - `Transport` → `"transport"` (`class.rs:120`); `Unparseable` →
     `"unparseable"` (`class.rs:121`)
   - non-Step outcomes: `Ok`→`"ok"`, `RfcAuditFail`→`"rfc_audit_fail"`,
     `CheckFail`→`"check_fail"`, `Panic`→`"panic"` (`class.rs:110-114`, `59-68`)

2. **case discriminator** (the refining sub-bucket) — `CallOutcome::case`,
   `class.rs:162-177`. For a `Step(e)` it is:
   ```rust
   format!("{}@{}", step_who(e), last_phase.unwrap_or("start"))   // class.rs:166
   ```
   `step_who` returns the `who` field of every variant (`class.rs:183-193`);
   `last_phase` is `ctx.phases().last()` fed by the driver
   (`crates/loadgen/src/driver.rs:618`). The result is `slug`'d — `@`, `-`, `.`,
   `_`, `+` survive; everything else → `-` (`class.rs:210-220`).

So the **full sample path** of a NOK call is
`<scenario_id>/<class-label>/<who>@<last_phase>`. Example: `abandon_ringing`'s
terminal (below) → `abandon_ringing / timeout / alice-abandoned-after-ringing@start`.

`ResultClass::chaos_excusable` (`class.rs:97-99`) additionally gates near/clear
auto-excusal: everything **except** `Panic`/`Unparseable` is excusable — the
actor port changes none of this.

---

## 2. Settle-verdict mapping (decision 3, plan §4.3 — fixed HERE)

The actor runner produces a `CallVerdict`
(`crates/scenario-harness/src/actor/mod.rs:66-77`):

| `CallVerdict`              | `into_result` maps to                                    |
|---------------------------|----------------------------------------------------------|
| `Ok`                      | `Ok(())`                                                 |
| `Failed(StepError)`       | `Err(e)` — the SAME `StepError` the linear body returned |
| `Settle(open: Vec<String>)` | **`Err(StepError::Timeout { who: "settle".to_string() })`** |

The `Settle(open)` → `Timeout{who:"settle"}` mapping is a **fixed decision**, not
something to discover: the driver's bucketing is untouched, so a settle-ceiling
breach lands in `class = timeout`, case `settle@<last_phase>`. `who = "settle"`
is a new bounded label — deliberately not a role — so a leaked-obligation
timeout is visually distinct in the sample tree from a wire timeout
(`alice`/`bob`). The `open` obligation names are for the sample `detail` /
`describe_open` (`actor/settle.rs:59`), never the case key.

---

## 3. `mark_ringing` — the cross-call 18x gate (absent from v1)

`ctx.mark_ringing(bool)` (`crates/scenario-harness/src/realcall/env.rs:465-467`)
folds one call's 18x outcome into the cross-call
`loadgen_ringing_{received,expected}_total` rate, gated at **>0.99**
(`crates/loadgen/src/report.rs:342`, HELP text + `record_ringing`
`report.rs:188-195`). The driver calls `reporter.record_ringing(ctx.ringing())`
at **`crates/loadgen/src/driver.rs:595`** (the plan's cited line — confirmed). A
lost non-PRACK 18x is a *rate*, not a per-call failure, so a call that reports
`Some(false)` is not itself NOK.

**Exactly two observations feed it (nowhere else calls `mark_ringing`):**

- `establish` — `env.rs`… → `mod.rs:133`: `ctx.mark_ringing(saw_ringing)`, where
  `saw_ringing` = *alice received the non-PRACK `180 Ringing`* (`Ok` arm of
  `call.try_expect(180)`, `mod.rs:125-132`). A `Timeout` on the 180 →
  `Some(false)` (tolerated); any other error → hard fail.
- `complete_100rel` — `mod.rs:302`: `ctx.mark_ringing(true)` **unconditionally**,
  right after alice receives the RELIABLE `183` (`p183`, `mod.rs:298-299`). A
  reliable provisional is guaranteed-delivery, so it always counts as received.

**Actor-world obligation:** reproduce `mark_ringing` at those two observation
points ONLY. The hand-rolled bodies that see a 180 but never call `mark_ringing`
— `refer` (180 at `refer.rs:52`), `refer_charlie_reject` (180 at
`failures.rs:144`), `abandon_ringing` (180 at `failures.rs:80`) — must **stay
absent from the gate** (see §8). The actor plan must not "helpfully" start
counting refer's 180, or the gate denominator shifts and the >99% ratio drifts.

---

## 4. Phase vocabulary (the full set, all bodies)

Phases are stamped by `ctx.phase(name)` — `env.rs:452-454`, unconditional, `name`
is `'static` (bounded). The last-reached phase keys the case suffix (§1) and the
chaos near/clear classifier consumes the whole trail
(`driver.rs:609-614`). "Trigger" is the message/observation immediately
preceding the stamp.

| phase            | fires after (observation / driven step)                           | source (shared or body)                    |
|------------------|-------------------------------------------------------------------|--------------------------------------------|
| `connected`      | bob (or winning callee) **received** the ACK — basic establishment | `mod.rs:149` (establish); `mod.rs:326` (complete_100rel) |
| `pracked`        | alice **received** `200 (PRACK)`                                   | `mod.rs:311` (complete_100rel)             |
| `keepalive_ack`  | alice **received** `200 (OPTIONS)` — the FIRST in-dialog OPTIONS ping | `long_call.rs:44`; `options_hold.rs:47` (first only) |
| `referred`       | alice/bob leg **received** `202 (REFER)`                          | `refer.rs:97`                              |
| `transferred`    | charlie actor **sent** its `200 (INVITE)` for the transfer leg    | `refer.rs:105`                             |
| `rerouted`       | bob2 **received** the SUT's rerouted INVITE (post bob-486 failover) | `rerouting_prack.rs:73`                    |
| `reinvited`      | alice **received** `200` to the delayed-offer re-INVITE           | `reinvite.rs:46`                           |
| `updated`        | alice **received** `200 (UPDATE)`                                 | `prack_update.rs:65`                       |
| `bye_200`        | alice **received** `200 (BYE)` — shared teardown only            | `mod.rs:361` (hangup_on)                   |

Notes:
- **Observation vs driven-step:** all are stamped by the driving/receiving test
  side after a specific message resolves. `transferred` is the one stamped after
  a **peer actor's send** (charlie's 200), not after receiving a SUT message —
  in the actor world its barrier predicate must key on charlie having emitted the
  transfer 200, not on an inbound SUT message.
- `keepalive_ack` fires on an **observation** (first OPTIONS-200), NOT a barrier
  — the plan's explicit example (`long_call.rs:44`). `options_hold` guards it
  with a `first` flag so the looped pings stamp it exactly once
  (`options_hold.rs:37,45-49`).
- **`long_call` does NOT stamp `bye_200`** — it tears down inline with a `quiesce`
  drain, not `hangup` (`long_call.rs:64-69`); its last phase is `keepalive_ack`.
- The failing bodies (`invite_reject`, `abandon_ringing`, `refer_charlie_reject`)
  stamp **no** phase — their last phase is `None` → `"start"` in the case key.

---

## 5. Per-body contract tables

Shared helpers referenced below (`crates/scenario-harness/src/realcall/mod.rs`):
`establish` (`:104-151`), `admitted_uas` (`:163-237`), `establish_100rel`
(`:254-269`), `complete_100rel` (`:288-328`), `hangup`/`hangup_on`
(`:335-363`). "Incidental NOK" = an error a `?`-propagated shared step can
surface (variant + `who`); "intended NOK terminal" = the failure the body is
*designed* to produce. `who` on every agent-driven step is that agent's role
name (`self.agent.name`, e.g. `agent.rs:2100,2114,2144`).

### 5.1 `basic_call` — happy path (`scenarios/basic_call.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Incidental NOK:** any `?` from `establish`/`hangup` — `Timeout`/`QueueClosed`/
  `WrongStatus`/`UnexpectedKind` with `who ∈ {alice, bob}` (e.g. a shed 503 →
  `WrongStatus{who:"alice",expected:180,got:503}`, `mod.rs:217-222`).
- **Phases (in order):** `connected` (`mod.rs:149`) → `bye_200` (`mod.rs:361`).
- **Anchors — `LOAD_CALL_ANCHORS`** (`registry.rs:379-385`):
  `initialInvite`←bob's rx INVITE (`mod.rs:115`); `firstProvisional`←alice's rx
  180, **optional** (only in the Ok arm, `mod.rs:126-128`); `answer`←alice's rx
  200 (`mod.rs:142`); `ack`←bob's rx ACK (`mod.rs:148`); `bye`←bob's rx BYE
  (`mod.rs:356`).
- **Checkpoints:** `time_to_200` (`mod.rs:143`), `time_to_bye_200` (`mod.rs:360`).
- **mark_ringing:** `establish`, on alice's 180 (`mod.rs:133`).

### 5.2 `reinvite` — happy path (`scenarios/reinvite.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Incidental NOK:** shared-step `?` (`who ∈ {alice, bob}`); plus the re-INVITE
  leg — `env.bob.try_receive("INVITE")` → `Timeout{who:"bob"}`, `reinv.try_expect(200)`
  → `WrongStatus{who:"alice"}` (`reinvite.rs:41-44`).
- **Phases:** `connected` → `reinvited` (`reinvite.rs:46`) → `bye_200`.
- **Anchors — `LOAD_REINVITE_ANCHORS`** (`registry.rs:387-394`): the
  `LOAD_CALL_ANCHORS` five **plus** `reInvite`←bob's rx re-INVITE
  (`reinvite.rs:42`).
- **Checkpoints:** `time_to_200`, `time_to_reinvite_200` (`reinvite.rs:45`),
  `time_to_bye_200`.
- **mark_ringing:** `establish` (`mod.rs:133`).

### 5.3 `refer` — happy blind transfer (`scenarios/refer.rs`)
- **Expect:** `HappyBye` (transfer merged, A BYE tears down all legs).
  **Intended terminal:** `Ok(())`.
- **Incidental / guard NOK (all bounded `who`):**
  - guard `UnexpectedKind{who:"refer", detail:"REFER scenario bound without a
    charlie leg"}` (`refer.rs:39-42`);
  - guard `UnexpectedKind{who:"refer", detail:"no charlie for Refer-To"}`
    (`refer.rs:80-83`);
  - hand-rolled establishment/teardown `?`: `Timeout`/`WrongStatus`/`UnexpectedKind`
    with `who ∈ {alice, bob, charlie}` (`refer.rs:49-65,95,100,123-145`).
- **Phases:** `referred` (`refer.rs:97`) → `transferred` (`refer.rs:105`). **No
  `connected`/`bye_200`** — establishment and teardown are hand-rolled, not via
  `establish`/`hangup`.
- **Anchors — `LOAD_REFER_ANCHORS`** (`registry.rs:398-404`):
  `initialInvite`←bob's rx INVITE (`refer.rs:50`); `firstProvisional`←alice's rx
  180 — **NOT optional here** (`?`-gated at `refer.rs:52`, then anchored `:53`);
  `answer`←alice's rx 200 (`refer.rs:60`); `ack`←bob's rx ACK (`refer.rs:65`);
  **`refer`←bob's SENT REFER** via `anchor_sent` (`refer.rs:91` — the plan's
  cited line; `sent:true`, resolution matches the entry's *sender*,
  `anchors.rs:86-91`). Plus a **second `initialInvite`** ←charlie's rx transfer
  INVITE (`refer.rs:101`, published as `charlie.initialInvite`). No `bye` anchor
  (teardown scenario-owned).
- **Checkpoints:** `time_to_200` (`refer.rs:61`), `time_to_202` (`refer.rs:96`),
  `time_to_charlie_200` (`refer.rs:104`).
- **mark_ringing:** **NONE** — refer's hand-rolled 180 does not feed the gate
  (see §3, §8).

### 5.4 `options_hold` — happy keepalive hold (`scenarios/options_hold.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Incidental NOK:** shared `?` (`who ∈ {alice, bob}`); loop step
  `env.bob.try_receive("OPTIONS")` → `Timeout{who:"bob"}`, `opt.try_expect(200)`
  → `WrongStatus{who:"alice"}` (`options_hold.rs:43-44`).
- **Phases:** `connected` → `keepalive_ack` (`options_hold.rs:47`, first ping
  only) → `bye_200`.
- **Anchors — `LOAD_CALL_ANCHORS`** (same five as basic_call). OPTIONS pings are
  NOT anchored.
- **Checkpoints:** `time_to_200`, `time_to_options_200` (`options_hold.rs:46`,
  first only), `time_to_bye_200`.
- **mark_ringing:** `establish` (`mod.rs:133`).

### 5.5 `long_call` — happy long-lived hold (`scenarios/long_call.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Incidental NOK:** shared `establish` `?`; one OPTIONS ping
  `env.bob.try_receive("OPTIONS")` → `Timeout{who:"bob"}`, `opt.try_expect(200)`
  → `WrongStatus{who:"alice"}` (`long_call.rs:41-42`); `bye.try_expect(200)` →
  `WrongStatus{who:"alice"}` (`long_call.rs:67`).
- **Phases:** `connected` → `keepalive_ack` (`long_call.rs:44`). **No `bye_200`**
  — inline `quiesce` teardown (`long_call.rs:64-69`); last phase is
  `keepalive_ack`.
- **Anchors — `LOAD_ESTABLISH_ANCHORS`** (`registry.rs:407-408`):
  `initialInvite`, `firstProvisional` (optional), `answer`, `ack` — establishment
  only. **No `bye` anchor** (tolerant quiesce teardown absorbs bob's BYE).
- **Checkpoints:** `time_to_200`, `time_to_options_200` (`long_call.rs:43`),
  `time_to_bye_200` (`long_call.rs:69`).
- **mark_ringing:** `establish` (`mod.rs:133`).

### 5.6 `prack_update` — happy 100rel + UPDATE (`scenarios/prack_update.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Incidental NOK:** `establish_100rel`/`complete_100rel` `?` — a lost reliable
  183 IS a failure here (no timeout tolerance): `p183 = call.try_expect(183)` →
  `WrongStatus{who:"alice"}` (`mod.rs:298`); PRACK leg `Timeout{who:"bob"}`
  (`mod.rs:306`); UPDATE leg `env.bob.try_receive("UPDATE")` → `Timeout{who:"bob"}`,
  `update.try_expect(200)` → `WrongStatus{who:"alice"}` (`prack_update.rs:61-63`);
  `hangup` `?` (`who:"bob"`/`"alice"`).
- **Phases:** `pracked` (`mod.rs:311`) → `connected` (`mod.rs:326`) → `updated`
  (`prack_update.rs:65`) → `bye_200`.
- **Anchors — `PRACK_ANCHORS`** (`registry.rs:410-417`):
  `initialInvite`←bob rx INVITE (`mod.rs:267`); `firstProvisional`←alice rx 183
  (`mod.rs:299`, not optional); `prack`←bob rx PRACK (`mod.rs:307`); `answer`←alice
  rx 200 (`mod.rs:319`); `ack`←bob rx ACK (`mod.rs:325`); `bye`←bob rx BYE
  (`mod.rs:356`). **UPDATE is NOT anchored** (not in the anchor vocabulary,
  `shape.rs:11-20`).
- **Checkpoints:** `time_to_prack_200` (`mod.rs:310`), `time_to_200`
  (`mod.rs:320`), `time_to_update_200` (`prack_update.rs:64`), `time_to_bye_200`.
- **mark_ringing:** `complete_100rel`, `true` on the reliable 183 (`mod.rs:302`).

### 5.7 `rerouting_prack` — happy failover + 100rel (`scenarios/rerouting_prack.rs`)
- **Expect:** `HappyBye`. **Intended terminal:** `Ok(())`.
- **Guard / incidental NOK:**
  - guard `UnexpectedKind{who:"rerouting_prack", detail:"bound without a bob2 leg"}`
    (`rerouting_prack.rs:44-47`);
  - first leg `admitted_uas(..,183)` `?` (`who:"alice"` on a shed final);
    `env.bob.try_receive("ACK")` for the 486 → `Timeout{who:"bob"}`
    (`rerouting_prack.rs:68`); bob2 leg `bob2.try_receive("INVITE")` →
    `Timeout{who:"bob2"}` (`rerouting_prack.rs:71`); `complete_100rel` `?`
    (`who ∈ {alice, bob2}`); `hangup_on(..,bob2)` `?` (`who:"bob2"`/`"alice"`).
- **Phases:** `rerouted` (`rerouting_prack.rs:73`) → `pracked` (`mod.rs:311`) →
  `connected` (`mod.rs:326`) → `bye_200`.
- **Anchors — `PRACK_ANCHORS`** (`registry.rs:410-417`): **`initialInvite`
  twice** — bob's rx (rejected) INVITE (`rerouting_prack.rs:64`) AND bob2's rx
  (winning) INVITE (`rerouting_prack.rs:72`); then via `complete_100rel`:
  `firstProvisional`←alice rx 183 (`mod.rs:299`), `prack`←bob2 rx PRACK
  (`mod.rs:307`), `answer`←alice rx 200 (`mod.rs:319`), `ack`←bob2 rx ACK
  (`mod.rs:325`); `bye`←bob2 rx BYE (`hangup_on` callee=bob2, `mod.rs:356`).
- **Checkpoints:** `time_to_prack_200`, `time_to_200`, `time_to_bye_200`.
- **mark_ringing:** `complete_100rel`, `true` on bob2's reliable 183 (`mod.rs:302`).

### 5.8 `invite_reject` — voluntarily-failing (`scenarios/failures.rs:27-59`)
- **Expect:** `Reject(486)`. **INTENDED NOK terminal:**
  `WrongStatus { who:"alice", expected:200, got:486, reason:"Busy Here" }`
  (from `call.try_expect(200)`, `failures.rs:53`; returned `r.map(|_|())` `:57`).
  → class **`status_486`**, case `alice@start`.
- **Incidental NOK:** `env.bob.try_receive("INVITE")`/`("ACK")` →
  `Timeout{who:"bob"}` (`failures.rs:40,45`).
- **Phases:** none. **Anchors:** none (descriptor has no `.anchors(...)`,
  `registry.rs:471-473`). **Checkpoints:** none. **mark_ringing:** none.
- **Teardown side-effect:** `scope.mark_terminated()` on the ≥200 final
  (`failures.rs:54-56`) → driver teardown is a no-op.

### 5.9 `abandon_ringing` — voluntarily-failing (`scenarios/failures.rs:65-106`)
- **Expect:** `AbandonedEarly`. **INTENDED NOK terminal (SYNTHETIC `who`):**
  ```rust
  Err(StepError::Timeout { who: "alice-abandoned-after-ringing".to_string() })  // failures.rs:104
  ```
  → class **`timeout`**, case `alice-abandoned-after-ringing@start`. Reproduce
  byte-for-byte.
- **Incidental NOK:** the CANCEL handshake steps —
  `env.bob.try_receive("INVITE"|"CANCEL"|"ACK")` → `Timeout{who:"bob"}`,
  `call.try_expect(180|487)`/`cxl.try_expect(200)` → `WrongStatus{who:"alice"}`
  (`failures.rs:78-100`).
- **Phases:** none. **Anchors:** none (`registry.rs:474-476`).
- **Checkpoints:** `time_to_180` (`failures.rs:81`).
- **mark_ringing:** **none** — sees a 180 (`failures.rs:80`) but never calls it.
- **Teardown side-effect:** `scope.mark_terminated()` after both legs torn down
  (`failures.rs:101`).

### 5.10 `refer_charlie_reject` — voluntarily-failing (`scenarios/failures.rs:114-183`)
- **Expect:** `TransferDeclined`. **INTENDED NOK terminal (SYNTHETIC `who`):**
  ```rust
  Err(StepError::UnexpectedKind {
      who: "refer_charlie_reject".to_string(),
      detail: "transfer declined by charlie (603)".to_string(),
  })                                                            // failures.rs:178-181
  ```
  → class **`unexpected`**, case `refer_charlie_reject@start`. Reproduce
  byte-for-byte (the `detail` is NOT keyed — free-form).
- **Guard NOK (same `who`, distinct detail — bounded because the who is fixed):**
  - `UnexpectedKind{who:"refer_charlie_reject", detail:"bound without a charlie leg"}`
    (`failures.rs:133-136`);
  - `UnexpectedKind{who:"refer_charlie_reject", detail:"no charlie for Refer-To"}`
    (`failures.rs:154-157`).
- **Incidental NOK:** hand-rolled A↔B establishment + REFER/202 `?` —
  `who ∈ {alice, bob, charlie}` (`failures.rs:139-166`).
- **Phases:** none. **Anchors:** none (`registry.rs:477-480`).
- **Checkpoints:** `time_to_200` (`failures.rs:147`).
- **mark_ringing:** none (sees a 180 at `failures.rs:144`, never calls it).
- **Teardown side-effect:** leaves scope **Confirmed** so driver teardown BYEs
  the still-live A↔B.

---

## 6. The anchor contract (verbatim arrays)

From `crates/e2e-model/src/registry.rs:374-426`:

```rust
// registry.rs:374-385
const CALL_ANCHORS: &[Anchor] = &[Anchor::InitialInvite, Anchor::Answer, Anchor::Ack, Anchor::Bye];
const LOAD_CALL_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
// registry.rs:387-394
const LOAD_REINVITE_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::ReInvite,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
// registry.rs:398-404  (hand-rolled establishment + SENT REFER on bob + charlie.initialInvite)
const LOAD_REFER_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Refer,
];
// registry.rs:407-408  (establishment only; tolerant quiesce teardown → no bye)
const LOAD_ESTABLISH_ANCHORS: &[Anchor] =
    &[Anchor::InitialInvite, Anchor::FirstProvisional, Anchor::Answer, Anchor::Ack];
// registry.rs:410-417
const PRACK_ANCHORS: &[Anchor] = &[
    Anchor::InitialInvite,
    Anchor::FirstProvisional,
    Anchor::Prack,
    Anchor::Answer,
    Anchor::Ack,
    Anchor::Bye,
];
```

Shape→anchor bindings (`registry.rs:435-499`): `basic_call`/`options_hold`/
`basic_call_em`→`LOAD_CALL_ANCHORS`; `reinvite`/`reinvite_em`→`LOAD_REINVITE_ANCHORS`;
`refer`→`LOAD_REFER_ANCHORS`; `long_call`→`LOAD_ESTABLISH_ANCHORS`;
`prack_update` & `rerouting_prack`→`PRACK_ANCHORS`. The failure shapes
(`invite_reject`, `abandon_ringing`, `refer_charlie_reject`) declare **no**
anchors.

**The attach-time rule (decision 5 — labels-on-barriers is insufficient).** An
anchor is an `AnchorKeys` extracted *from the specific message the agent is
holding at that instant* — `CallCtx::anchor(agent, name, keys: impl Into<AnchorKeys>)`
(`env.rs:402-404`); `AnchorKeys` is `From<&SipRequest>`/`From<&SipResponse>`
(`crates/scenario-harness/src/anchors.rs:42-64`), capturing Call-ID, CSeq
seq+method, request-method/response-status, and the top Via branch. Resolution
re-finds the recorded entry by those keys + the agent's address (or *sender*
address for a `sent` anchor, `anchors.rs:86-101`). Therefore an anchor **must be
attached at reaction / goal-drive time, with the message in hand** — a phase
barrier holds no message, so barrier labels alone cannot mint an anchor. Two
consequences the actor port must honour:

- `firstProvisional` is captured **only when the 180 actually arrived** — inside
  the `Ok` arm of `establish` (`mod.rs:126-128`), the "optional block" the
  registry comment calls out (`registry.rs:377-378`). In the reliable-provisional
  path it is unconditional (the 183 is guaranteed, `mod.rs:299`).
- The REFER anchor is `anchor_sent` (`refer.rs:91`): the SUT is its only
  receiver, so it matches on bob's *sender* address (`AnchorTag.sent = true`,
  `env.rs:408-410`, `anchors.rs:86-91`). No test agent ever receives it, so a
  receive-side barrier could never anchor it.

---

## 7. Emergency + dual-body notes

- `basic_call_em` / `reinvite_em` (`registry.rs:462-469`) reuse the SAME
  `BasicCall` / `Reinvite` bodies with `.emergency()` — identical contract table
  to §5.1 / §5.2 (the emergency marker only affects admission on the wire,
  `env.rs:299-302`; it does not change any `who`/phase/anchor). Name them in the
  P3 parity set (plan §6).
- `rerouting_prack` is the one dual-body descriptor
  (`registry.rs:494-497`): the load body is §5.7; the functional body lives in
  `e2e-core`. Only the load body is in scope for this port.

---

## 8. Verification notes (plan assumption vs code)

Every synthetic `who` and cited line the plan named was checked against source.

**Confirmed exactly as the plan stated:**
- `Timeout{who:"alice-abandoned-after-ringing"}` **is at `failures.rs:104`** ✓.
- `UnexpectedKind{who:"refer_charlie_reject"}` terminal **is at `failures.rs:178`**
  ✓ (the `Err(...)` starts line 178; struct fields 179-180).
- `anchor_sent` for the REFER **is at `refer.rs:91`** ✓.
- `keepalive_ack` on the first OPTIONS-200 **is at `long_call.rs:44`** ✓.
- `LOAD_*_ANCHORS`/`PRACK_ANCHORS` **span `registry.rs:374-426`** ✓.
- `mark_ringing` 18x gate is `record_ringing(ctx.ringing())` at
  **`driver.rs:595`** ✓; `run_one` spans ~`driver.rs:388-659` (plan cited
  `525-618`/`453-565` — the fn is larger than either range but both cited spans
  fall inside it).
- `AnchorKeys`-from-a-message: the `anchor` fn signature taking
  `keys: impl Into<AnchorKeys>` **is at `env.rs:402`** ✓ (the `AnchorKeys` type +
  its `From` impls live in `anchors.rs:32-73`).

**Discrepancies / refinements to flag for the port:**

1. **`refer_charlie_reject` has THREE call sites with `who:"refer_charlie_reject"`,
   not one.** Besides the plan-cited terminal (`failures.rs:178`), the two guards
   at `failures.rs:134` and `failures.rs:154` use the same `who`. All three are
   `UnexpectedKind` → class `unexpected`, case `refer_charlie_reject@start`, so
   they *collapse to one case bucket* — but the port must keep `who` identical on
   all three, not just the terminal.

2. **`refer` similarly has two guard sites with `who:"refer"`** (`refer.rs:39`,
   `refer.rs:80`) in addition to its happy path — bounded, must be preserved.

3. **`rerouting_prack` guard uses `who:"rerouting_prack"`** (`rerouting_prack.rs:44`)
   — the same "scenario-id-as-who" pattern; add it to the synthetic-`who`
   inventory (the plan named only the `refer_charlie_reject` and `abandon_ringing`
   synthetics).

4. **`mark_ringing` is NARROWER than "any 18x observation".** It is called in
   exactly two places — `establish` (`mod.rs:133`) and `complete_100rel`
   (`mod.rs:302`). The three hand-rolled bodies that receive a 180
   (`refer` `refer.rs:52`, `refer_charlie_reject` `failures.rs:144`,
   `abandon_ringing` `failures.rs:80`) **do NOT feed the gate**. The actor port
   must reproduce this asymmetry exactly, or the >0.99 gate denominator shifts.

5. **`long_call` does not stamp `bye_200`** (inline quiesce teardown,
   `long_call.rs:64-69`) — its terminal phase is `keepalive_ack`. Any actor
   barrier plan that assumes every happy body ends at `bye_200` would mis-key
   `long_call`'s case suffix on an incidental teardown failure.

6. **UPDATE and OPTIONS are never anchored** — `prack_update`'s UPDATE and the
   OPTIONS pings in `options_hold`/`long_call` produce no anchor (the vocabulary,
   `shape.rs:11-20`, has no UPDATE/OPTIONS member). Do not invent one.

7. **The case key does NOT embed the `StepError` variant name** — the variant
   selects `ResultClass` (the *class* label / parent dir), and the case is
   `who@phase` only (`class.rs:166`). The plan's phrase "case key (variant +
   step_who + last phase)" spans the two levels; stated precisely here so the
   port does not concatenate the variant into the case string.

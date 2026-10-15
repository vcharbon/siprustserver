# The HA tooling is a pluggable base

**Status:** accepted (2026-10-15)

**Owner:** `tooling/hakit/` (the runner, its schemas in `tooling/hakit/schemas/`,
its own content in `tooling/hakit/content/`), `deploy/helm/siprust/` and
`deploy/helm/siprust-observability/` (the charts), `crates/loadgen` (the
product-shape seam and the outcome records it writes),
`crates/metric-catalogue` and `crates/metric-contract` (the catalogue a
product extends).

## Context

The endurance and chaos tooling grew as shell around the lab.
`deploy/k8s/chaos.sh` (853 lines) and `endurance.sh` (1637) hold every fault
primitive, the load streams, the gates and the tolerances in one place. The
only way to extend them is to source `chaos.sh` as a library and override its
functions. Four things went wrong with that shape.

- **The damage was judged per script, after the fact.** Each event pushes
  `sip_chaos_event{type,phase|result}` and is gated by its own rule (a pass
  threshold, an excused class). Nothing says what impact an event may cause,
  so a gate cannot tell an expected residual from a defect, and failures
  between events are counted by no gate.
- **The judge was blind to single calls.** SIPp reports counters only
  (`deploy/k8s/sipp/exporter/`). A call a fault stranded, or a handful of
  failures under a rate rule, cannot be attributed to anything. loadgen
  classes its calls (`crates/loadgen/src/class.rs`) and excuses some of them
  as `near` a fault (`chaos.rs`), a second acceptance mechanism beside the
  gates.
- **Variants were made by rewriting files.** Scenario variants are separate
  XML copies, or are patched with `sed` at launch. Manifests are rendered by
  `envsubst`, which blanked a shell variable in the proxy's init script. A
  consumer forks the manifests and dashboards it needs to change.
- **Overriding shell functions does not scale to a second downstream
  product.** A product with its own faults, load shapes and impact classes
  would be coupled to every function name and global of the base, and would
  fork what it cannot override.

## Decision

The HA tooling is one pluggable base: a Python runner that discovers incidents,
impact budgets, run profiles, scenarios, dashboards, treatment catalogs and
metric catalogues on a search path, runs a profile against the lab, and judges
every call by a versioned outcome record. A product plugs in by adding
directories to the search path. It never forks a file of the base. Every seam
is cut for a second product whose code the base does not know.

### 1. The search path

1. `HAKIT_PATH` is a colon-separated list of directories. The search path is
   `tooling/hakit/content/` followed by the `HAKIT_PATH` entries in order.
2. Each entry may hold:

   | directory | holds | schema |
   |---|---|---|
   | `incidents/<name>/` | `incident.yaml`; for a fault, executables `inject`, `heal`, optional `probe` | `incident.v1` |
   | `budgets/<name>.yaml` | one impact budget | `budget.v1` |
   | `profiles/<name>.yaml` | one run profile | `profile.v1` |
   | `catalogs/<name>.json` | one treatment catalog | `treatment-catalog.v1` |
   | `classes/<name>.yaml` | one added impact class, beside its executable `<name>.count` | `impact-class.v1` |
   | `metrics/<name>.json` | metric catalogues in the `metric-contract export` format | the export format |
   | `scenarios/` | SIPp scenario files and their injection files | SIPp's own |
   | `dashboards/` | Grafana dashboard JSON | Grafana's own |

3. A name is unique across the whole path: an incident, budget, profile,
   catalog, class or shape id defined twice is refused, naming both files. A
   product changes what a base incident does through parameters and targets in
   its profile, never by shadowing it. Silent shadowing would make the same
   profile name mean different runs depending on `HAKIT_PATH`.
4. Every file is validated against its schema before anything runs
   (`hakit validate`, and the first step of every other subcommand). A file
   that fails is refused with the path and the schema error. What a schema
   cannot express (a parameter default of the declared type where the schema
   leaves it open, a profile's timing bounds, a stream's shape in a catalog)
   the runner checks at the same step.
5. Data is the contract and code is the leaf. Where an incident or a class
   needs logic, its directory carries an executable in any language; the
   runner knows it only through the contract below.
6. Every schema is versioned. The major version is in the file and record
   name (`call-outcome.v1`); a breaking change is a new major file beside the
   old one, and the runner refuses a major it does not ship. An added optional
   field is a minor version of the same file (its `$comment`), read by every
   reader of that major.

### 2. The incident contract

An incident says what happens and to what. It never says what impact is
tolerated (§3).

1. **Layout.** `incidents/<name>/incident.yaml` (fields in
   `incident.v1.schema.json`: `kind` `fault|load`, a fault's `target`
   selector and pick and its `params`, a load incident's `stop`,
   `duration_s`, `expected_s`, `timeouts`, `requires`). A fault has the
   executables `inject` and `heal`, and optionally `probe`. A load incident
   has none but an optional `probe` (§2.8).
2. **Phases.** For each event the runner records, in order: `inject` (before
   the inject step), `injected` (it succeeded), `heal` (after `duration_s`,
   before the heal step), `healed` (it succeeded), `recovered` (`probe`
   exited 0, or right after `healed` without a probe). A step that fails or
   overruns its timeout records `failed`. A combined event injects its
   incidents in the listed order and heals them in reverse; each incident
   records its own phases.
3. **Environment.** Each executable runs with its working directory set to
   its event directory and receives:

   | variable | value |
   |---|---|
   | `HAKIT_RUN`, `HAKIT_RUN_DIR` | the run id and its directory |
   | `HAKIT_EVENT`, `HAKIT_EVENT_DIR` | the event id (`<seq>-<incident>`, the names joined by `+` for a combined event) and this incident's directory under it |
   | `HAKIT_INCIDENT` | the incident name |
   | `HAKIT_TARGET_SELECTOR`, `HAKIT_TARGET_NAMESPACE`, `HAKIT_TARGET_PICK` | the target after the profile's override |
   | `HAKIT_KUBE_CONTEXT` | the kube context of the lab |
   | `HAKIT_DURATION_S` | the hold of this event |
   | `HAKIT_PARAM_<NAME>` | each parameter, the name upper-cased |
   | `HAKIT_LIB` | the directory of the base's shell fault primitives (`pod-faults.sh`) |
   | `HAKIT_VM_IMPORT` | the metrics push endpoint, for an executable that publishes its own facts |

4. **Output.** stdout is empty or one JSON object of facts (the pod picked,
   the instant of the kill, a restart timing); the runner keeps it in the
   event's record and in the verdict's `facts`. stderr goes to the event
   directory. State that `heal` needs from `inject` (the pod frozen, the rule
   added) is a file in `HAKIT_EVENT_DIR`.
5. **Exit codes.** 0 done. For `probe`, 75 (`EX_TEMPFAIL`) means not yet: the
   runner calls it again every `probe_interval_s` until `probe_s`. Anything
   else, or the timeout (the runner kills the process group), fails the step.
6. **Heal always runs.** The runner calls `heal` after a failed `inject`, on
   its own interruption, and from `hakit cleanup` for every event a crashed
   run left open. So `heal` is idempotent and correct after a partial
   `inject` or none.
7. **Backends.** The fault mechanism is the executable's choice: the shell
   primitives of `pod-faults.sh` (node stop, cgroup freeze, netem delay and
   loss, TCP reset), a Chaos Mesh resource it creates and deletes, `kubectl`.
   An incident that needs a backend names it in `requires`, and the runner
   refuses the profile before the run if the lab lacks it.
8. **Load incidents.** A load incident points at a profile stream with
   `start: incident` (the event's `stream`). The runner, never an executable,
   launches that stream at `inject` and stops it at `heal`, so the stream has
   its cps for `max_seconds`, its catalog and `load_safe` check, its
   `verdict.streams` entry and its counter reconciliation like any other.
   With `stop: drain` (the default) the generator stops placing calls, the
   calls in flight finish their teardown within the stream's
   `setup_deadline_s` or `ring_s` + `hold_s`, and only then is the generator
   removed (SIPp: `SIGUSR1`; loadgen: rate 0, then its drain). With
   `stop: abandon` the generator is killed with its calls in flight, which
   leaves those calls to the system under test to reap; the stream writes no
   record for them and is left out of the reconciliation. A stream that
   starts with an incident has no pre-inject baseline and so no `recovery`.
9. **The event series.** At each phase the runner pushes
   `sip_chaos_event{incident,phase,source,run} 1` to VictoriaMetrics with the
   phase instant as its timestamp. `source` is `schedule` (fired by the
   profile), `manual` (`hakit fire`) or `detected` (an event the runner saw
   happen, such as a generator that died). While a window is open the runner
   also pushes `sip_chaos_window{incident,run}` at 1 every 10 s and 0 when it
   closes, so a range query at any step sees the window. These series are for
   display. The verdict reads the run's own `events.jsonl`, which holds the
   same instants.
10. **The base incidents** in `tooling/hakit/content/incidents/` target pods
    by label, so a product points each at its own components:

    | incident | kind | what it does | replaces in `chaos.sh` |
    |---|---|---|---|
    | `kill_worker` | fault | force-delete one worker pod (grace 0) | `kill_worker` |
    | `kill_worker_graceful` | fault | delete one worker pod with its grace period | `kill_worker` with `KILL_GRACE` |
    | `kill_worker_held` | fault | delete one worker and keep it absent for `duration_s`, then let it start once | (new) |
    | `kill_proxy` | fault | force-delete the front proxy that holds the virtual address | `kill_proxy` |
    | `pod_kill` | fault | force-delete one pod of any selector | `limiter_kill` |
    | `pod_restart` | fault | delete one pod of any selector with its grace period | `limiter_restart` |
    | `pod_freeze` | fault | cgroup-freeze one pod for `duration_s` | `limiter_freeze`, `broker_freeze` |
    | `net_delay` | fault | netem delay on one pod for `duration_s` | `limiter_slow` |
    | `net_loss` | fault | netem loss on one pod for `duration_s` | `limiter_netcut` |
    | `tcp_reset` | fault | reset new TCP connections to one pod's port for `duration_s` | `limiter_reject` |
    | `cpu_starve` | fault | cap the CPU of one pod (pick `one`) or all (pick `all`) for `duration_s` | `cpu_starve`, `cpu_starve_all` |
    | `node_stop` | fault | stop the node that hosts a pod of the selector, start it at heal | `infra_node_kill` |
    | `partition` | fault | cut the network between two selectors for `duration_s` | (new) |
    | `cut_external_plane` | fault | take the external interface of the virtual-address holder down | `cut_external_plane` |
    | `spike` | load | a burst stream, drained at heal | `peak` |
    | `abuse` | load | a stream of an abuse scenario, drained at heal | `abuse_up`, `abuse_down` |
    | `orphan` | load | a stream abandoned mid-call at heal (`stop: abandon`) | `orphan_kill` |

    `overload_mix` and `overload_all` are combined events
    (`incidents: [cpu_starve, spike]`), not incidents.
11. **The spike rule.** A burst of new calls is something the system under
    test absorbs: the base's spike budget allows no `setup_failed` and no
    `unexpected_ending` on any stream, only `setup_slow` and `recovery`. An
    overload refusal (503) is budgeted only for incidents that model
    catastrophic load: a loop, a flood. A product's budgets follow the same
    rule.

### 3. The impact-budget contract

A budget says what impact a window allows. It never says how an incident is
made. A run profile joins the two.

1. **Impact classes.** The classifier derives these from outcome records (§4)
   and the shape's catalog entry (§5), each with an instant. A record is a
   success when its `ending` is in the shape's `expected` and every catalog
   leg opened and ended as its leg `expected` says.

   | class | from the record | instant | budget |
   |---|---|---|---|
   | `setup_failed` | the shape expects `answered` and the call ended before answer some other way: `rejected`, `abandoned`, `timeout`, or `failed` before answer (code 0). Keyed by stream × `final_code` (0 without a final) × `reason` | `setup_ended_at_ms`, else `ended_at_ms` | per key |
   | `unexpected_ending` | any other caller ending not in `expected` (`rejected` with another code, `answered` where only a reject is expected), and, when the caller's ending is expected, a catalog leg that never opened or ended otherwise (keyed with reason `leg:<role>`) | `setup_ended_at_ms`, else `ended_at_ms` | per key before answer; 0 after answer |
   | `setup_slow` | from `started_at_ms` to `first_provisional_at_ms` (to `setup_ended_at_ms` when no provisional came) above the threshold; ring time never counts | that instant | count |
   | `dialog_request_failed` | each `failures` entry of kind `dialog_request` | its `at_ms` | 0 |
   | `call_cut` | `ending: cut` | the failure that cut it, else `ended_at_ms` | 0 |
   | `call_stranded` | `ending: stranded` | `ended_at_ms` | 0 |
   | `protocol_anomaly` | each `failures` entry of kind `protocol` or `transport` | its `at_ms` | 0 |
   | `cdr_mismatch` | §3.8 | §3.8 | count |
   | `recovery` | §3.6 | `healed` | seconds |
   | `unattributed` | an impact of a budgetable class whose instant is outside every window, or an in-dialog impact with no candidate window | its instant | 0, never budgeted |

   One record may yield several impacts (a re-INVITE answered 481 is a
   `dialog_request_failed`, and the call it ended is a `call_cut`). A record
   whose `failures` hold a `generator` entry is not an impact of the system
   under test: it is a `generator_failure` integrity finding.

2. **Nothing is accepted by default.** A class or key a budget does not name
   allows 0. A `setup_failed` or `unexpected_ending` code a budget does not
   name is a finding: the code is never a wildcard, because a new failure
   code is exactly what a run exists to show. The in-dialog classes
   (`dialog_request_failed`, `call_cut`, `call_stranded`,
   `protocol_anomaly`) and an `unexpected_ending` after answer take no budget
   at all, and the budget schema has no place for them: an established call
   is what HA protects, and a call answered where the treatment should have
   refused it is a treatment that stopped working.
3. **Keys.** A `setup_failed` or `unexpected_ending` entry names `stream` (or
   `*`, each stream separately), `code`, `reason` (or `*`) and either
   `max_calls` or `max_seconds`. `max_seconds` is the allowance in seconds of
   the stream's offered rate, so one budget holds at any cps. A failed setup
   is charged to the first entry that matches it.
4. **The reason vocabulary.** With a final, `reason` is the first value of its
   first Reason header line normalised to `<protocol>;cause=<n>`, the protocol
   token lowercased (`q.850;cause=34`), or the status code alone (`486`) when
   that first value does not read, states no decimal cause or names a protocol
   outside `[a-z0-9.+-]+`; a later value never stands in, and the reason phrase
   is never matched. Without a final it is one of `timeout` (no final by the setup
   deadline), `transport` (a send or receive failed) or `protocol` (the
   generator stopped the call on a message it could not accept). SIPp and
   loadgen use the same spellings.
5. **Windows.** An event's window opens at its first `inject` instant and
   closes `settle_s` after its last `recovered` instant. A budgetable impact
   belongs to the window that holds its instant; one outside every window is
   `unattributed`. An in-dialog impact takes no budget, so its attribution is
   for analysis: its `candidate_windows` are every window whose `inject` fell
   inside the call's lifetime before the failure, and with none it is
   `unattributed`. A call stranded before a window opens and whose setup
   deadline falls inside it is charged to that window by its instant. A
   window whose inject, heal or probe step fails closes at the incident's
   maximum (every timeout and `duration_s`, plus `settle_s`) with status
   `incident_failed`, which makes the run `invalid`.
6. **Recovery** is computed from the records, so a verdict recomputes from the
   run directory alone. The baseline of a stream is its successful-setup
   ratio and its p99 setup time (as `setup_slow` measures it) over
   `baseline_s` before `inject`. Traffic is back when, over `hold_s` in a row,
   every stream's ratio is within `tolerance` below its baseline and its p99
   within `tolerance` above. Ratios, not counts, so a rate change from
   `steps` inside a window is no impact. `recovery_s` is the time from
   `healed` to the start of that span; a budget's `recovery.max_s` caps it,
   and without one traffic must be back before the window closes. A stream
   with no baseline (one that starts with an incident) has no recovery.
7. **Timing bounds.** `settle_s` is at least the longest `setup_deadline_s`
   of the streams, so a call stranded at the fault reaches its deadline
   inside the window; `drain_s` is at least max(`setup_deadline_s`,
   `ring_s` + `hold_s`) of every stream. The runner refuses a profile that
   breaks either. The dry-run checks that no two windows overlap using each
   event's `expected_s` (default `duration_s` + 60) plus `settle_s`; with the
   defaults (`settle_s` 60) a kill every 3 minutes passes. At run time a
   window that opens while another is open is a `window_overlap` integrity
   finding.
8. **CDR mismatch.** The CDR consumer's output reaches the run directory as
   `cdrs.jsonl`, one `cdr-observation.v1` line per CDR (call_id, start,
   answer, end, duration, its unit, disposition). Joined on `call_id` with
   the records: an answered call with no CDR is charged at its `ended_at_ms`;
   a CDR with no record at its own `end_ms`; a CDR whose disposition differs
   from the record's, or whose duration differs from `ended_at_ms` −
   `answered_at_ms` by more than 1 s plus the CDR's unit, at the record's
   `ended_at_ms`.
9. **Events off the schedule.** `hakit fire <incident>` during a run and a
   `detected` event (named after what was detected, such as `stream_died`)
   open windows under the same rules, closing at their `recovered` instant or
   at the incident's maximum. Their budget, parameters and target come from
   the profile's `defaults` entry for that incident. `hakit fire` without an
   entry, or while a window is open, is refused; a detected event without an
   entry opens a window that allows nothing.
10. **Added classes.** A product adds a class with `classes/<name>.yaml` and
    its executable `classes/<name>.count`, which reads the run directory and
    prints the impacts with their instants. Attribution and budgets work on it
    as on a built-in class (`budget.classes.<name>.max`).
11. **The verdict.** `verdict.json` (`verdict.v1`): per stream the calls, the
    calls never placed, the records and the successes; per window its source,
    status, every phase as `{phase, at_ms, step}`, the facts, each stream's
    recovery and every budgetable impact with its count, its seconds of
    offered rate, its allowance and whether it is a finding; the in-dialog
    impacts with their candidate windows; the unattributed impact; the
    integrity findings. `status` is `invalid` when an integrity finding
    exists, else `fail` when any impact is a finding, else `pass`. A human
    `summary.md` is rendered from it.

### 4. The call-outcome record

1. **One record per call placed.** Every caller-side generator writes one
   `call-outcome.v1` record (`tooling/hakit/schemas/call-outcome.v1.schema.json`)
   when it is done with a call, on every exit path: success, failure,
   timeout, abort. The record states the caller's facts (`started_at_ms`,
   `first_provisional_at_ms`, `setup_ended_at_ms`, `answered_at_ms`,
   `ended_at_ms`, `phase_reached`, `final_code`, `reason`, `ending`,
   `early_media`, `failures`, `legs`) and never a judgement. The generator's
   own class goes in `producer_verdict`, which the classifier does not read,
   so a generator cannot excuse a call.
2. **SIPp.** The UAC writes the record with its `<log>` action into a file in
   the stream's stats directory. The exporter twin follows the file, ships
   each line and counts the records by `stream`, `shape`, `ending`. A SIPp UAS
   writes no record; it exposes counters only. SIPp ends a call without
   running any step of the scenario in three cases: the global
   `-recv_timeout` deletes the call, an exhausted retransmission of a `<send>`
   without `ontimeout` aborts it, and an unexpected message received while
   the call is already in its unexpected-message handler aborts it. So every
   UAC scenario follows these rules:
   - every `<recv>` has its own `timeout` with an `ontimeout` label, or is on a
     path that reaches a label that writes the record;
   - every `<send>` SIPp retransmits has `ontimeout`;
   - the `_unexp.main` handler accepts any request and any response (a
     regexp `response` match). A response there is one that landed while the
     call stood on a step other than a recv, and SIPp discards a byte-identical
     copy of the message it last received, so the handler acts on it as the
     missed step would have (a provisional is stamped, an INVITE final answers
     or rejects the call, a 2xx retransmission is ACKed again) and the call
     resumes. A request the scenario cannot serve writes the record and ends
     the call, its dialog torn down first when answered;
   - the handler's resume applies the `next` of the step before the one it
     returns to, so a label a jump reaches never follows a step with an
     unconditional `next`;
   - every abort path the scenario can reach writes a record before it ends;
   - `-recv_timeout` is above every timeout of the scenario, a backstop that
     never fires on a correct scenario.

   A static lint checks these rules on every scenario of the search path, and
   `hakit validate` runs it.
3. **loadgen** writes the same record natively for each call it placed,
   mapped from its result classes:

   | loadgen class | record |
   |---|---|
   | `ok` | the observed ending (`answered`, `rejected` with its code, `abandoned`); the branch in `producer_verdict` |
   | `status_<code>` | before answer `rejected` with `final_code`; after answer a `dialog_request` failure with `code` |
   | `timeout` | before answer `ending: timeout`; after answer a `dialog_request` failure without `code`, or `stranded` on the BYE |
   | `wrong_method`, `unexpected`, `unparseable`, `rfc_audit_fail`, `check_fail` | a `protocol` failure, `ending: failed` (reason `protocol` before answer) if it stopped the call |
   | `transport` | a `transport` failure, `ending: failed` (reason `transport` before answer) |
   | `panic` | a `generator` failure, `ending: failed` |
   | `rejected` (`key_in_flight`, `key_cooling`, …), or shed at the in-flight cap | no record: the call was never placed. Counted per stream and shape in a `not_placed` counter; neither an impact nor a generator failure |
   | a panic outside the scenario's boundary, no INVITE sent, a refused bind | no record: the generator failed the call. Counted per stream and shape in a `generator_failures` counter; the run is `invalid` |

   The facts of a record are sealed when the call's verdict resolves, before
   the generator tears down a failed call: the teardown's CANCEL and the 487 it
   draws, a final after the step deadline, or a cleanup BYE change no ending;
   of the teardown only whether a BYE released the dialog is kept.

   A record's `ending` comes from what the caller observed, never from an
   accepted branch's name. loadgen's `near`/`clear` tag and its `POST /chaos`
   marker play no part in the verdict: attribution is the window's (§3.5).
4. **Transport.** A record reaches VictoriaLogs as one line on
   `/insert/jsonline` with `_stream_fields=run,source,stream` and `_time` set
   from `ended_at_ms`. VictoriaLogs is the live view. At the end of a run the
   runner exports the run's records into the run directory, and the verdict
   reads that copy, so a verdict can be recomputed after VictoriaLogs
   retention has dropped the run.
5. **Reconciliation.** For each stream, the records plus the calls not placed
   plus the calls the generator failed equal the delta of the generator's own
   offered-call counter (every slot of its rate, shed included) over the
   stream's life, read after the drain. A difference is a `record_gap` integrity
   finding: an exit that writes no record would otherwise hide exactly the
   calls that went wrong.
6. **The classifier** is one module of the runner,
   `tooling/hakit/src/hakit/classify.py`: a pure function of a record and its
   catalog shape to impacts with instants. Neither generators nor dashboards
   classify.

### 5. The treatment catalog and the product-shape seam

1. A treatment catalog (`treatment-catalog.v1`) maps each shape id to how a
   caller requests the treatment (`request`: the R-URI user part and header
   fields, with `${name}` placeholders the consumer fills), the callee-side
   legs it opens (`legs`: a role, the R-URI prefixes that select it on the
   generator's callee socket, the leg endings expected), the caller endings
   that count as success (`expected`) and whether it is `load_safe`.
2. The runner, the SIPp injection-file renderer, loadgen and the classifier
   read the same catalog, so the treatment a stream asks for and the ending it
   is judged by cannot drift apart. An expected non-2xx is success: a shape
   that fails over from a primary route expects `rejected` with its code on
   that leg.
3. The base's own catalog (`tooling/hakit/content/catalogs/x-api-call.json`)
   requests treatments with the `X-Api-Call` header the development decision
   backend reads (`crates/b2bua/src/decision/test_adapter.rs`), with the
   placeholders `${callee_host}` and `${callee_port}`. It is the reference
   implementation. A product with another way of asking (a dialed-number
   plan) exports its own catalog to its search-path entry.
4. **loadgen's product-shape seam.** loadgen is a library whose entry point
   takes a shape registry (`ShapeRegistry`); a product binary registers its
   shapes and adds no wiring of its own. A product shape registers under its
   catalog shape id: one id is one catalog entry and one registered shape.
   loadgen lists the ids it registered, and the dry-run refuses a `loadgen`
   stream whose shape has no registration or no catalog entry.

### 6. The runner

1. **Language.** Python, the package `hakit` at `tooling/hakit/` (pyproject
   and a uv lockfile; the only dependencies are PyYAML and a JSON Schema
   validator, pinned). pyright strict and pytest run in the verify gates.
2. **Subcommands.** `hakit validate`, `hakit up [--profile P]` (the cluster
   and the charts, §7), `hakit run <profile>`, `hakit fire <incident>` (one
   manual event in the running run), `hakit cleanup` (heal every open event),
   `hakit verdict <run-dir>`. `--dry-run` applies to all of them.
3. **Injected clock and executor.** Every wait goes through a clock and every
   effect (an executable, `kubectl`, `helm`, `docker`, an HTTP push or query)
   through an executor. `--dry-run` runs the whole profile on a fake clock and
   a fake executor that records each call and returns scripted results, and
   prints the timeline with every window. A test runs every profile on the
   search path that way in seconds.
4. **The run directory.** `$HAKIT_RESULTS/<run>/`, the run id
   `<profile>-<UTC yyyymmddThhmmssZ>`: the resolved profile, `run-meta.json`
   (the tree that ran, the host facts), `events.jsonl`, `events/<event>/`,
   the exported records, `cdrs.jsonl`, `verdict.json`, `summary.md`. The run
   id is also the nonce the generators put in their Call-IDs, so two runs
   never share one.
5. **Before the load.** The runner records the host (free memory, swap, disk,
   load) and refuses the run outside the profile's `preflight` limits; it
   refuses to start while the system under test holds calls (a
   `leftover_calls` finding) or while an event of an earlier run is open.
6. **During the load.** Every series the run reads live is checked for
   freshness; one that stops updating for three scrape intervals is a
   `series_stale` integrity finding with its instant. A generator that exits
   before its stream is due to stop is `stream_died`, and a `detected` event.
7. **The fault primitives stay shell** behind the incident contract. The
   readers that judge a soak today move into the package with their product
   terms made parameters, and are wired to the verdict where they overlap.

### 7. Deployment, dashboards and metrics

1. **Charts.** `deploy/helm/siprust/` deploys the system under test and its
   generic backends: the front proxy, the workers, the call limiter, RabbitMQ
   and the CDR consumer, each with `image`, `extraEnv`, `extraContainers`,
   `extraVolumes` and `values.schema.json`, and each disabled by a value. A
   product changes the image and adds its components in its own umbrella
   chart that depends on this one. It never copies a manifest.
2. `deploy/helm/siprust-observability/` deploys the in-cluster collectors
   (vmagent, kube-state-metrics, node-exporter, log shipping), pointed at the
   host stack by value. `hakit up` installs it on every lab.
3. **The host stack stays.** VictoriaMetrics, VictoriaLogs and Grafana run in
   the host compose stack (`deploy/observability/stack/`), so the history of
   a run survives the teardown of its cluster.
4. **Lab values come from the runner.** `hakit up` passes the kind-specific
   values (`--set` and values files the profile names). No manifest is
   rendered by `envsubst`.
5. **Dashboards.** The base owns every generic panel; a product dashboard
   holds only its product rows and links to the base dashboards with the time
   range carried. Grafana provisions every `dashboards/` directory of the
   search path in place, without staging copies. Every dashboard carries the
   generic annotation block: window regions from `sip_chaos_window == 1` and
   phase points from `sip_chaos_event`, titled by incident and phase.
   `hakit validate` refuses a dashboard on the path without it.
6. **Series labels.** Every series a generator exposes and every record carry
   `stream` and `shape`, so the generic panels show a product's shapes
   without product panels.
7. **Metric catalogue.** A product extends the contract through `metrics/`:
   catalogue files in the `metric-contract export` JSON format. The name
   checker reads the base's export plus every `metrics/` file of the search
   path. The series the tooling itself pushes (`sip_chaos_event`,
   `sip_chaos_window`) and the generators' record and not-placed counters are
   declared in the base's `content/metrics/`, not allow-listed.

## Considered and rejected

- **Sourcing a shell library and overriding functions.** It couples a
  consumer to every function name and global of the base, and a second
  product would inherit the same coupling.
- **Plug-in code (Python entry points) instead of directories.** A product
  would have to write Python against the runner's internals for what is data
  in almost every case. Executables cover the remainder in any language.
- **Tolerances inside the incident.** The same kill then needs a copy per
  tolerance, and two products cannot share a fault while disagreeing on what
  it may cost.
- **Judging from counters.** Counters cannot place a call in a window or join
  it with a CDR, and a burst of a few calls hides under a rate rule.
- **An incident executable that starts its own load.** Its stream would have
  no cps, catalog check, reconciliation or verdict entry; the runner already
  knows how to start and drain a stream.
- **A Rust runner.** The runner shells out, waits and parses YAML; the work is
  orchestration. Python with pyright strict and a fake executor gives the
  same refusal of untested paths at a fraction of the build cost, and the
  readers it absorbs are Python already.
- **A deterministic in-process incident engine first.** Real kills, freezes
  and partitions on the lab find what the in-process harness cannot model.
  An incident that exposed a real defect is later reproduced in the harness
  through the same incident spec.

## Consequences

- One cutover deletes the old tooling together once the runner, the charts
  and the base content cover it: `chaos.sh`, the endurance scripts, the
  stream launcher of `sipext-gen.sh` where the runner replaces it, the raw
  manifests and their `envsubst` rendering. The old tooling takes no fix
  until then.
- `sip_chaos_event{type,…}` with `result` is replaced by
  `sip_chaos_event{incident,phase,source,run}`; the dashboards' annotation
  queries change with it.
- The base's SIPp scenarios (`deploy/k8s/sipp/scenarios/`) and Grafana
  dashboards (`deploy/observability/stack/grafana/dashboards/`) move into
  `tooling/hakit/content/scenarios/` and `content/dashboards/`, so the base
  is the first search-path entry like any other.
- SIPp scenarios are deduplicated with SIPp-native parameters (`-key`, `-d`,
  injection files); a renderer exists only for attribute-level variants, and
  nothing rewrites a scenario with `sed` or `perl`. The bench reads the same
  scenarios.
- Every SIPp UAC scenario gains a record at every exit and passes the lint of
  §4.2; the exporter twin gains the record shipping and counter.
- A product's dashboards, metric families, incidents, budgets and profiles
  live in its own repository and reach the base only through `HAKIT_PATH`.

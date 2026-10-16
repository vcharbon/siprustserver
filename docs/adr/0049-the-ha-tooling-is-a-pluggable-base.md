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
   reader of that major. Until the first production release a v1 file may
   still be narrowed in place when every producer already complies.

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
   | `HAKIT_LIB` | the directory of the base's shell fault primitives (`tooling/hakit/lib/`: `pod-faults.sh` and the incident helpers) |
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
   `inject` or none. The first SIGTERM or SIGHUP makes the runner ignore
   every later one while it heals and removes its generators: a signal to its
   process group reaches it a second time through a wrapper that forwards it.
7. **Backends.** The fault mechanism is the executable's choice: the shell
   primitives of `pod-faults.sh` (node stop, cgroup freeze, netem delay and
   loss, TCP reset), a Chaos Mesh resource it creates and deletes, `kubectl`.
   An incident that needs a backend names it in `requires`, and the runner
   refuses the profile before the run if the lab lacks it. A capability whose
   backend the lab can show is read from the lab, never declared: `chaos-mesh`
   is there when the API server serves `networkchaos.chaos-mesh.org`. The base
   uses Chaos Mesh (installed on the kind lab by `deploy/helm/kind-lab.sh`,
   unless `CHAOS_MESH=0`) for what the shell primitives cannot do precisely: a
   partition between pods, a volume's IO latency, a clock shift. Its resource
   is named after the run, event and incident and created, never applied over
   one that exists; its `AllInjected` condition is the applied proof and its
   deletion through Chaos Mesh's finalizer the undone proof. Chaos Mesh
   refuses host-network pods, so a network fault on the front proxy stays
   netem, refused unless the profile allows it (`allow_host_network`: a
   host-network pod's netns is its node's); so does a fault restricted to one
   port (`sip_loss`). A step concludes only from a positive answer: a read
   that fails is a failed step (a not-yet in a probe), never an empty answer.
   A crash kills the container's task by its id and proves it dead by the
   runtime's state; a heal proves its fault gone, and again a second later.
   A step that overruns its timeout gets SIGTERM on its process group and a
   grace to stop what it started, then SIGKILL. A fault heal that fails is
   retried once; one that fails twice stops the schedule there, drains the
   load and makes the run `invalid`, so no later window is judged on a system
   under test that may still be faulted.
8. **Load incidents.** A load incident points at a profile stream with
   `start: incident` (the event's `stream`). The runner, never an executable,
   launches that stream at `inject` and stops it at `heal`, so the stream has
   its cps for `max_seconds`, its catalog and `load_safe` check, its
   `verdict.streams` entry and its counter reconciliation like any other.
   With `stop: drain` (the default) the generator stops placing calls, the
   calls in flight finish their teardown within the stream's
   `setup_deadline_s` or `ring_s` + `hold_s`, and only then is the generator
   removed (`SIGUSR1` for both drivers; loadgen also takes `POST /drain`,
   and a drained loadgen must exit 0, any other exit fails the step). With
   `stop: abandon` the generator is killed with its calls in flight, which
   leaves those calls to the system under test to reap; the stream writes no
   record for them and is left out of the reconciliation. A stream that
   starts with an incident has no pre-inject baseline and so no `recovery`.
   Each firing is its own generator, its Call-IDs tagged with the event; at
   heal its records are shipped and its counter read before it is removed,
   and the stream's entry sums its firings. `healed` is the instant the
   generator exited, so the window holds the drained calls. A drain signal
   docker refuses, or a drain that overruns the incident's `heal_s`, kills
   the generator and fails the step (the signal is never sent twice: a second
   `SIGUSR1` makes SIPp exit at once);
   the runner refuses a profile whose stream needs longer to drain than the
   `heal_s` of the load incident that stops it, and a stream stopped both
   ways.
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
    | `kill_worker` | fault | crash one worker: SIGKILL its processes from the node, proven dead; the kubelet restarts it | `kill_worker` |
    | `kill_worker_graceful` | fault | delete one worker pod with its grace period | `kill_worker` with `KILL_GRACE` |
    | `kill_worker_held` | fault | crash one worker and keep it absent for `duration_s` (the nodes it may run on tainted NoSchedule), then let it start once | (new) |
    | `kill_proxy` | fault | crash the front proxy that holds the virtual address | `kill_proxy` |
    | `pod_kill` | fault | crash one pod of any selector | `limiter_kill` |
    | `force_delete` | fault | force-delete one pod through the API, its processes left running: two incarnations at once | `kill_worker`'s former mechanism |
    | `pod_restart` | fault | delete one pod of any selector with its grace period | `limiter_restart` |
    | `pod_freeze` | fault | freeze one pod's pod-level cgroup for `duration_s` | `limiter_freeze`, `broker_freeze` |
    | `net_delay` | fault | netem delay on one pod for `duration_s` | `limiter_slow` |
    | `net_loss` | fault | netem loss on one pod for `duration_s` | `limiter_netcut` |
    | `tcp_reset` | fault | reset new TCP connections to one pod's port for `duration_s` | `limiter_reject` |
    | `cpu_starve` | fault | cap the CPU of one pod (pick `one`) or all (pick `all`) for `duration_s` | `cpu_starve`, `cpu_starve_all` |
    | `node_stop` | fault | stop the nodes `node_selector` names, or the node that hosts a pod of the selector; start them at heal | `infra_node_kill` |
    | `partition` | fault | cut the network between the targets and their peers for `duration_s` (Chaos Mesh); by default one worker from the others, the replication link | (new) |
    | `sip_loss` | fault | netem loss, duplication and delay on one pod's SIP datagrams only, for `duration_s` | (new) |
    | `io_stall` | fault | delay the file operations of one pod's volume for `duration_s` (Chaos Mesh) | (new) |
    | `clock_skew` | fault | shift one pod's wall clock for `duration_s` (Chaos Mesh) | (new) |
    | `cut_external_plane` | fault | take the external interface of the virtual-address holder down | `cut_external_plane` |
    | `spike` | load | a burst stream, drained at heal | `peak` |
    | `abuse` | load | a stream of an abuse scenario, drained at heal | `abuse_up`, `abuse_down` |
    | `orphan` | load | a stream abandoned mid-call at heal (`stop: abandon`) | `orphan_kill` |

    A crash kills the process: an API force-delete leaves the elder running
    beside its replacement for the seconds its kubelet takes to stop it, two
    incarnations owning the same calls. That shape is `force_delete`'s, which
    measures the overlap; a crash's probe proves it is 0.

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
   | `setup_failed` | the shape expects `answered` and the call ended before answer some other way: `rejected`, `abandoned`, `timeout`, or `failed` before answer (code 0). Keyed by stream × `final_code` (0 without a final) × `reason` | `setup_ended_at_ms`, else `ended_at_ms`; one outside every window belongs to the first window open while it was pending (§3.5) | per key |
   | `unexpected_ending` | any other caller ending not in `expected` (`rejected` with another code, `answered` where only a reject is expected), and, when the caller's ending is expected, a catalog leg that never opened or ended otherwise (keyed with reason `leg:<role>`) | `setup_ended_at_ms`, else `ended_at_ms`; a leg of an answered call at `ended_at_ms`, when it is known missing; a consult's contract refusal (§3.2) at the refusal | per key before answer; 0 after answer but a consult's contract refusal |
   | `setup_slow` | from `started_at_ms` to `first_provisional_at_ms` (to `setup_ended_at_ms` when no provisional came) above the threshold; ring time never counts; a setup that drew no response at all takes none (it is a `setup_failed`) | that instant | count |
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
   under test: it is a `generator_failure` integrity finding. Nor is a
   measurement the generator took while it stalled (§3.12).

2. **Nothing is accepted by default.** A class or key a budget does not name
   allows 0. A `setup_failed` or `unexpected_ending` code a budget does not
   name is a finding: the code is never a wildcard, because a new failure
   code is exactly what a run exists to show. The in-dialog classes
   (`dialog_request_failed`, `call_cut`, `call_stranded`,
   `protocol_anomaly`) and an `unexpected_ending` after answer take no budget
   at all, and the budget schema has no place for them: an established call
   is what HA protects, and a call answered where the treatment should have
   refused it is a treatment that stopped working. One exception: a catalog
   leg declared `after_consult` opens only after an in-dialog consult of a
   decision service that another leg (`by`) asks for, and the catalog states
   the answers the consult's contract gives `by` when that service fails
   (`refusals`: method, subscription state, body prefix). On an answered call
   with no `failures` entry, that leg unopened while `by` received one of
   them is the consult's failure: an `unexpected_ending` keyed `leg:<role>`,
   code 0, at the refusal's instant, which a window's budget may allow.
   Unopened with any other answer, or with any other failure in the call, it
   stays an in-dialog impact; the call's other impacts and its CDRs are
   judged as any call's. The generator records what each leg received
   in dialog for this (`legs[].received`), and a leg's own in-dialog request
   that failed as a `failures` entry naming the leg (`leg`), so a consult
   that never ran (its request refused by a hop) is named as such.
3. **Keys.** A `setup_failed` or `unexpected_ending` entry names `stream` (or
   `*`, each stream separately), `code`, `reason` (or `*`) and either
   `max_calls` or `max_seconds`. `max_seconds` is the allowance in seconds of
   the stream's offered rate, so one budget holds at any cps. An entry may
   add `retry_after` (true: only a final that carried a Retry-After; false:
   only one that carried none), so a refusal asking the caller to retry later
   is never charged to the allowance of a bare refusal of the same code, and
   `over_outage`, which counts `max_seconds` beyond the window's outage (its
   first `inject` to its last `recovered`), so one budget states a fault's
   residue whatever its hold; `setup_slow` takes `over_outage` the same way.
   A failed setup is charged to the first entry that matches it.
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
   closes `settle_s` after the later of its last `recovered` instant and the
   instant every stream with a baseline is back (§3.6), at most at its limit:
   the incident's maximum (every timeout and `duration_s`, plus `settle_s`)
   or the next window's opening, whichever comes first. While a stream is not
   back the window closes at that limit. The `sip_chaos_window` series show the window up to
   `recovered` + `settle_s`, the only close the runner knows live; the
   verdict's window is the one above. A budgetable impact belongs to the
   window that holds its instant; a setup that failed outside every window
   (`setup_failed`, or an `unexpected_ending` before answer: timed out, or a
   final that came late) belongs to the first window open while it was
   pending (an INVITE sent into the fault, ended after the window closed);
   one outside every window is `unattributed`. An in-dialog impact takes no budget, so its attribution is
   for analysis: its `candidate_windows` are every window whose `inject` fell
   inside the call's lifetime before the failure, or that holds the failure's
   instant, and with none it is `unattributed`. A call stranded before a window opens and whose setup
   deadline falls inside it is charged to that window by its instant. A
   window whose inject, heal or probe step fails closes at the incident's
   maximum with status `incident_failed`, which makes the run `invalid`.
6. **Recovery** is computed per stream from the records, so a verdict
   recomputes from the run directory alone. A call's setup succeeded when its
   record yields no budgetable impact; its setup time is what `setup_slow`
   measures. The baseline of a stream is the successful-setup ratio and the
   nearest-rank p99 setup time of its calls started in the `baseline_s`
   before `inject`, leaving out the calls started inside an earlier window.
   A baseline needs 20 calls and its p99 20 setup times; a stream short of
   them (one that starts with an incident, or whose baseline an earlier
   window took) has no recovery, and the window's facts (`hakit` /
   `recovery`) say why. From `healed`, in steps of 1 s, the first span of
   `hold_s` holding at least 20 calls (by `started_at_ms`), a ratio of at
   least the baseline's × (1 − `tolerance`) and a p99 (over at least 20
   setup times) of at most max(baseline × (1 + `tolerance`), baseline +
   `latency_slack_ms`) is where the stream is back (`latency_slack_ms`
   default 50: a relative tolerance alone is jitter on a setup of a few
   milliseconds). The runner refuses a budget whose `hold_s` holds fewer than
   20 calls of a stream at its lowest rate. Ratios, not counts, so a rate
   change from `steps` is no impact. `recovery_s` is the time from `healed`
   to the start of that span; a budget's `recovery.max_s` caps it, and
   without one the stream must be back before the window's limit (§3.5). A
   measurement taken across a generator stall (§3.12) counts in neither the
   baseline nor a span.
7. **Timing bounds.** `settle_s` is at least the longest `setup_deadline_s`
   of the streams plus 1 s (the tick at which a generator checks its
   deadline), so a call stranded at the fault reaches its deadline inside
   the window; `drain_s` is at least max(`setup_deadline_s`,
   `ring_s` + `hold_s`) of every stream. The runner refuses a profile that
   breaks either. The dry-run checks that no two windows overlap using each
   event's `expected_s` (default `duration_s` + 60) plus `settle_s`; with the
   defaults (`settle_s` 60) a kill every 3 minutes passes. A window of the
   verdict that opens while another is open is a `window_overlap` integrity
   finding.
8. **CDR mismatch.** A profile with `cdr_join: true` joins the run's CDRs with
   its records, on a lab that offers the capability `cdr`. The base's CDR
   consumer (`cdr-consumer-runner`, with `CDR_OBSERVATION_URL` set) ships one
   `cdr-observation.v1` line per CDR it consumes to VictoriaLogs the way the
   generators ship records (`source` `cdr-consumer`); a product whose CDR
   payload differs builds the consumer from the same library with its own
   observation view, and counts the payloads its view cannot read
   (`cdr_observations_unread_total`). After the drain the runner exports
   the observations whose `call_id` carries the run id (the Call-ID nonce of
   §6.4) into `cdrs.jsonl`, once the consumer has consumed every CDR the
   system under test wrote (the lab's written and consumed counters over the
   run) and the shipped observations have stopped growing for 10 s, at most
   300 s, long enough for a broker thawing late. The consumer acknowledges a
   CDR only once its observation has landed, so a consumer killed in between
   leaves the CDR to the broker's redelivery. `cdrs-collect.json` keeps
   whether that happened and the counts. Byte-identical observations (a
   redelivery, a re-post) count once. Joined on `call_id`, each mismatch of a
   kind:
   - `missing`: an answered call with no CDR, charged at its `ended_at_ms`;
   - `extra`: a CDR with no record, at its own `end_ms`;
   - `duplicate`: each distinct CDR after the first of a call, at the
     record's `ended_at_ms`;
   - `differs`: a CDR whose disposition differs from the record's; for a
     record ending `answered` (a planned teardown, timed by the caller), a
     duration that differs from `ended_at_ms` − `answered_at_ms` by more than
     1 s plus the CDR's unit; for a `cut` or `stranded` call, an impact
     already, a CDR ending more than 1 s plus its unit after the record's
     `ended_at_ms` (the call billed past the caller's end); each at the
     record's `ended_at_ms`, with its excess past that tolerance.

   An unanswered call without a CDR is no mismatch, unless the profile's
   `cdr_required` names its ending (and optionally its final's code and
   whether that final carried a Retry-After): such a call is `missing`. The
   CDRs of a firing stopped with `abandon` (their Call-IDs carry its event)
   join the firing instead of records: each call it placed owes one CDR, and
   a second one of a Call-ID, fewer or more Call-IDs than calls placed are
   charged at the instant its generator exited. A missing CDR whose call
   ended outside every window but within the profile's `cdr_in_flight_s`
   before a window opened belongs to that window: its CDR was still in the
   sink when the fault hit; the value is stated from the system under test's
   publish and confirm timeouts, never fitted. Any other mismatch outside
   every window, of a call whose lifetime overlapped windows, takes no
   budget: it is listed in `in_dialog` with those windows as candidates (the
   window may have changed the CDR, the call ended after it). A `differs` whose caller
   answer or end instant fell in a generator stall of its stream was measured
   by the generator: it is listed in that stall's `generator_stall` finding,
   never charged. A window allows each kind separately
   (`cdr_mismatch.<kind>`): `max_calls`, or `max_seconds` of the summed
   offered rate of the run's `run` streams (calls end at the rate they start),
   with `over_outage` beyond the window's outage as for `setup_failed`; a
   `differs` allowance may add `excess_ms`, every duration past the join's
   tolerance by at most that much (a bound the budget derives, such as the
   retransmission timers of a lossy plane). What leaves the CDR measurement in
   doubt is a `class_failed` integrity finding, never impacts: an export cut
   with CDRs still owed (its missing CDRs are not charged), payloads the
   consumer could not read, observations off the schema, and no observation
   at all while answered calls exist.
9. **Events off the schedule.** `hakit fire <incident>` during a run and a
   `detected` event (named after what was detected, such as `stream_died`)
   open windows under the same rules. Their ids are `m<seq>-<incident>` and
   `d<seq>-<name>`. `hakit fire` leaves a request in the running run's
   directory (`fire/`) and waits for the answer; the runner takes it between
   two waits, refuses it without a `defaults` entry for the incident, while
   a window is open, or when its window (its `expected_s` + `settle_s`)
   would reach a scheduled inject, and otherwise runs the event with the entry's budget,
   parameters and target. A detected event heals nothing: its window opens at
   the detection and closes `settle_s` later, under the `defaults` entry of
   its name, or without one under a budget that allows nothing.
10. **Added classes.** A product adds a class with `classes/<name>.yaml` and
    its executable `classes/<name>.count`. After the records and CDRs are
    exported the runner runs each count once, with `HAKIT_RUN_DIR` set; it
    prints one JSON array of impacts with their instants, kept as
    `classes/<name>.json` in the run directory so the verdict recomputes
    without the executable. `run-meta.json` lists the classes of the search
    path the run expects. A count that fails or prints anything else, and an
    expected class without its kept answer, is a `class_failed` integrity
    finding. Attribution and budgets work on it as
    on a built-in class (`budget.classes.<name>.max` per window).
11. **The verdict.** `verdict.json` (`verdict.v1`): per stream the calls, the
    calls never placed, the records, the successes and the generator's
    stalls; per window its source, status, every phase as
    `{phase, at_ms, step}`, the facts, each stream's recovery and every
    budgetable impact with its count, its seconds of offered rate, its
    allowance and whether it is a finding; the in-dialog impacts with their
    candidate windows; the unattributed impact; the integrity findings.
    `status` is `invalid` when an integrity finding exists, else `fail` when
    any impact is a finding, else `pass`. A human `summary.md` is rendered
    from it: the status and why, then per window and per stream each class
    against its allowance, and each window's in-dialog impacts it is a
    candidate of.
12. **Generator health.** A generator stall is a time the generator did not
    run: it placed no call and read no message. It has two signals. In the
    records: two consecutive calls of a stream started more than 1/cps +
    `generator_stall_ms` apart (profile, default 500 ms, SIPp's own watchdog
    threshold), at the lowest rate in force over the gap; the stall runs from
    the start that was due (the first start + 1/cps) to the second start. A
    gap is also what a generator at its call limit shows, the system under
    test holding calls: the runner sizes SIPp's limit for every call of the
    peak rate at the scenario's worst length, and a gap while the generator's
    statistics show the limit reached is no stall. In the generator's own
    statistics: the interval between two rows across which its watchdog
    counter grew (SIPp's `WatchdogMinor`/`WatchdogMajor`). A setup time
    measured across a stall, and a setup deadline (the call's start + the
    stream's `setup_deadline_s`) that falls inside one, measure the generator:
    they leave the impacts and the recovery samples, and are
    listed, with their Call-IDs, in one `generator_stall` integrity finding
    per stall. A stall that spoiled no measurement is listed in the stream's
    `stalls` and leaves the status alone. A generator's own count of calls it
    failed without a record (loadgen's `loadgen_generator_failures_total`) is
    a `generator_failure` integrity finding when above 0. A stall, a spoiled
    measurement or a timed finding that overlaps an interval where the host
    was under stress (§6.6), and a record gap of a stream with such a stall,
    make one `host_under_stress` finding per stressed span, listed first with
    its instants and its worst io and memory full share and swap-in rate, so
    a host stall is never investigated as a fault of the system under test
    or of the tooling.

### 4. The call-outcome record

1. **One record per call placed.** Every caller-side generator writes one
   `call-outcome.v1` record (`tooling/hakit/schemas/call-outcome.v1.schema.json`)
   when it is done with a call, on every exit path: success, failure,
   timeout, abort. The record states the caller's facts (`started_at_ms`,
   `first_provisional_at_ms`, `setup_ended_at_ms`, `answered_at_ms`,
   `ended_at_ms`, `phase_reached`, `final_code`, `reason`,
   `retry_after_s`, `ending`, `early_media`, `failures`, `legs`) and never a
   judgement. The first provisional is the first past 100 Trying, which is
   the next hop's own acknowledgement (RFC 3261 §8.2.6.1); `retry_after_s` is
   the delta-seconds of the final's Retry-After, absent without one. The generator's
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
   - every wait (a group of `<recv>`s) is entered on a recv with its own
     `timeout` and `ontimeout`: a pending send never bounds it (below);
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

   Facts of SIPp's engine (3.7, `call.cpp`) shape these rules:
   - the retransmissions of a pending send stop when a recv placed after it
     takes any final, an INVITE provisional, or an ACK, CANCEL, BYE or PRACK,
     another transaction's included: a scenario sends a request whose
     retransmissions such a message stopped again (a BYE), and every
     retransmitted send precedes `_unexp.main`;
   - SIPp sends nothing while a send of its own is retransmitting, so a
     request landing while the call's request is pending is captured by the
     wait and answered once a provisional or the final is in;
   - on a byte-identical copy of the message last received, SIPp re-sends the
     first message it sent after that receive, and only that one;
   - a `<recv response>` without `response_txn` takes only the responses to
     the CSeq methods of the `<send>`s placed before it in the file, so a wait
     follows the requests it waits for and `_unexp.main` follows every send;
   - a message landing while the call runs its handler aborts the call, so a
     handler path answers and resumes in one step (the resume is an action of
     its send); an `ontimeout` naming the end of the scenario counts the call
     failed; SIPp 3.7 loads no scenario file over 64 KiB.

   A static lint checks these rules on every scenario of the search path (the
   wait timers, the method lists, the handler's coverage, the resume guards,
   one record on every path, the file size), and `hakit validate` runs it. The
   base UAC is one file over the ~500-line ratchet: SIPp has no include, and
   one file keeps one copy of the record machinery.

   A scenario's variants are `-key` parameters. It declares the default of
   each key it reads beyond the runner's own (`run`, `stream`, `shape`,
   `hold_ms`, `setup_ms`) in its header comment, one line
   `hakit-key: <name>=<value>` each; the
   runner passes the defaults under the stream's `params`, and refuses a
   profile whose stream leaves a key the scenario reads unset. The base has
   one UAC (`uac-basic-hold.xml`: the hold, the caller's re-INVITEs, the far
   end's re-INVITEs and BYE, the ghost and flood abuse patterns) and one UAS
   (`uas-basic.xml`).
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
   accepted branch's name. Attribution is the window's (§3.5).
4. **Transport.** A record reaches VictoriaLogs as one line on
   `/insert/jsonline` with `_stream_fields=run,source,stream`, in one shape
   whatever the producer: every field of the record at top level, the record
   verbatim in `_msg`, and `_time` set from `ended_at_ms`
   (`tooling/hakit/tests/fixtures/shipped-record.json`, which each producer's
   tests check). VictoriaLogs is the live view. At the end of a run the
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
   generator's callee socket, the leg endings expected, and with `when` the
   caller endings under which the treatment opens the leg: a call refused
   before any leg opens judges none, and with `after_consult` the leg that
   asks for the in-dialog consult the leg waits on and that consult's
   contract refusals, §3.2), the caller endings that count as
   success (`expected`) and whether it is `load_safe`.
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
   loadgen lists the ids it registered, and the runner refuses a `loadgen`
   stream whose shape has no registration or no catalog entry. A `loadgen`
   stream's `params` name the product's `image` and its binary (`command`);
   every other parameter is one more flag of the binary, and the flags the
   runner sets (address, rate, run, stream, call limit as `--max-in-flight`,
   records file and URL) are refused there. Its counters are those of its
   last `/metrics` body, which loadgen leaves in the stream directory as it
   exits (`metrics.prom`).
5. **Generator placement.** A stream's generator is a docker container on the
   generator network (`placement: docker`, the default) and dials the lab's
   SIP target, or the stream's own `target` (`host:port`). A system under
   test the generator network cannot reach (cells exposed only as ClusterIP
   Services) takes `placement: pod`: the generator is a pod in the lab's
   namespace on the nodes of `HAKIT_POD_NODE_SELECTOR`, under the same
   contract. Its `gen` container is the binary bound to the pod's own
   address, its records shipped to VictoriaLogs as the pods reach it
   (`HAKIT_VLOGS_URL_PODS`); its `keeper` container (a POSIX shell of the same
   image) outlives the binary so the `/stats` both share is read after it
   exited; the drain is `POST /drain` and a rate step `POST /rate` through the
   API server's pod proxy; every file of `/stats` and the binary's log are
   copied into the stream directory before the pod goes; the pod is annotated
   for the in-cluster scrape. A `target` host that is not an IPv4 address
   names a Service of the namespace, resolved to its cluster IP when the
   stream starts. Only a loadgen stream runs as a pod: it plays its own
   callee legs, where a SIPp UAC's callees answer on the generator network.

### 6. The runner

1. **Language.** Python, the package `hakit` at `tooling/hakit/` (pyproject
   and a uv lockfile; the only dependencies are PyYAML and a JSON Schema
   validator, pinned). pyright strict and pytest run in the verify gates.
2. **Subcommands.** `hakit validate`, `hakit up [--profile P]` (the cluster
   and the charts, §7), `hakit run <profile>`, `hakit fire <incident>` (one
   manual event in the running run), `hakit cleanup` (heal every open event of
   a run whose runner is gone, and remove the generators, containers and
   pods, of every such run), `hakit verdict <run-dir>`, `hakit pcap <run-dir>
   <call-id>` (one call's packets out of the run's capture), `hakit mark` (the
   `sip_soak_*` state and Grafana annotations of a workflow supervising a
   run), and the readers (§6.7). `--dry-run` applies to all of them.
3. **Injected clock and executor.** Every wait goes through a clock and every
   effect (an executable, `kubectl`, `helm`, `docker`, an HTTP push or query)
   through an executor. `--dry-run` runs the whole profile on a fake clock and
   a fake executor that records each call and returns scripted results, and
   prints the timeline with every window. A test runs every profile on the
   search path that way in seconds.
4. **The run directory.** `$HAKIT_RESULTS/<run>/`, the run id
   `<profile>-<UTC yyyymmddThhmmssZ>`: the resolved profile, `run-meta.json`
   (the tree that ran, the host facts, the load's start once it started),
   `events.jsonl`, `events/<event>/`, `integrity.jsonl` (each finding appended
   as the runner finds it, the file rewritten whole at the end),
   `steps.jsonl` (the rate steps applied), `host.jsonl` (the host's
   pressure and swap, §6.6), the exported records,
   `cdrs.jsonl`, `verdict.json` (each impact row lists 50 Call-IDs; the full
   lists are in `impact-calls.jsonl`), `summary.md`, `verdict-tree.json` (the
   tree that computed the verdict, which may be later than the run's), and with
   a profile's `capture` the packet rings (`pcap/ring/<source>/`, on the
   callers' bridge and in the nodes it names) and, at each window's close, the
   ring files written since it opened (`pcap/windows/<event>/`). The run
   id is also the nonce the generators put in their Call-IDs, so two runs
   never share one. A generator never waits on the host's disk or swap: a
   docker generator works in `$HAKIT_GENERATOR_SCRATCH/<run>/` (tmpfs,
   `/dev/shm/hakit` by default), its files copied into the run directory
   once its containers are removed, every generator container runs with no
   swap and as the runner's user, and a pod generator's directory is
   memory-backed. A tmpfs page is charged to the memory of the container
   that wrote it, so each limit holds the files its container writes beside
   its process: the SIPp UAC its record file (about 0.5 KB a call) and the
   kept tails of its stat and error files, the exporter twin its
   `errors.kept.log` (64 MiB at most in 256 MiB), a pod generator its
   emptyDir (512 MiB at most in 1536 MiB). A firing whose files stay in the
   scratch is a `generator_failure`, and `hakit cleanup` copies out the
   scratch of every run whose runner is gone.
5. **Before the load.** The runner records the host (free memory, swap, disk,
   load) and refuses the run outside the profile's `preflight` limits; it
   refuses to start while the system under test holds calls (a
   `leftover_calls` finding), while an event of an earlier run is open, or
   while the host is under stress: io or memory full pressure (avg10) or the
   swap-in rate at or above the profile's `preflight` thresholds
   (`max_swapin_pages_s` 50, `max_memory_full_pct` 5, `max_io_full_pct` 20
   by default; stated, not fitted). Swap-in is the trigger: a generator
   waits on each fault of its own pages, and memory full pressure is reclaim
   every allocating task runs. A tmpfs-backed generator does not wait on the
   host's disk, so io full pressure counts only as a sustained stall risk.
6. **During the load.** The runner samples the host every second while it
   waits between steps, from the run's start to its verdict:
   `/proc/pressure/{cpu,memory,io}` (`some` and `full`, avg10 and the stall
   totals) and `/proc/vmstat` (`pswpin`, `pswpout`) into `host.jsonl`,
   pushed as `hakit_host_pressure_ratio` (the share of the interval stalled,
   from the totals: a step that blocks delays the next sample, and the
   longer interval still counts every stall in it, averaged over its span)
   and `hakit_host_swap_pages_per_second`. An
   interval at or above a `preflight` threshold is under stress (§3.12).
   Every series the run reads live is checked for
   freshness every scrape interval (15 s): each stream's records in
   VictoriaLogs, whose count must grow once the stream's first calls could
   have ended, and each VictoriaMetrics selector of the lab's `live_series`
   (`HAKIT_LIVE_SERIES`, `;`-separated; the system under test's call gauge
   by default), whose newest sample must be recent. One that stops updating
   for three scrape intervals (for records, at least five calls of the
   stream's rate) is a `series_stale` integrity finding with the instant it
   was last fresh. A generator that exits
   before its stream is due to stop is `stream_died`, and a `detected` event.
   A `run` stream's rate `steps` are applied at their instants (SIPp's remote
   control `cset rate`, loadgen's `POST /rate`) and logged in `steps.jsonl`;
   a step the generator refuses is a `generator_failure`. A load incident's
   stream takes no steps.
7. **The fault primitives stay shell** behind the incident contract. The
   readers that judge a soak today move into the package with their product
   terms made parameters, and are wired to the verdict where they overlap.
   They are `hakit` subcommands over a run directory (`hakit/readers/`): what
   is a product's (pods, containers, bounds, series scopes) is a flag or a
   `--spec` YAML the product keeps in its own repository, never a search-path
   family. `drift` judges memory, CPU per call, setup latency per shape (from
   the records, a setup measured across a generator stall left out), the
   gauges after the drain (at 0, or for a gauge traffic outside the load
   holds, such as a proxy's probe transactions, back to its median over the
   two minutes before the
   load within a stated tolerance) and one CDR outcome per call, on samples outside
   the verdict's windows, and exits non-zero on a `fail` or `na`; `chart` draws a range query with the
   run's windows and phases; `logs` reads every container incarnation's lines
   back from VictoriaLogs; `obs-check` checks the series of named scopes;
   `probe` is the live snapshot of a run, running or ended: each window's
   live status from `events.jsonl`, the integrity findings appended so far,
   the records of the recent span classified and judged provisionally
   against the windows (a window not yet extended to the instant traffic was
   back, calls in flight missing), a second final after an INVITE's 2xx, the
   system under test's live calls, the generators' offered rate by stream,
   the restarts and a product's bounds, then the anomalies; `ramp-table`
   builds a capacity ramp's step rows from a run's `steps` (the records of
   each step's calls, the worker's counters and CPU from VictoriaMetrics),
   and its step judgement, limits and CPU fit are pure functions over them.

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
   a run survives the teardown of its cluster. It scrapes the docker
   generators at their slot addresses (the SIPp exporter twins and the
   loadgen streams); a pod-placed generator is scraped in the cluster.
4. **Lab values come from the runner.** `hakit up` passes the kind-specific
   values (`--set` and values files the profile names). No manifest is
   rendered by `envsubst`.
5. **Dashboards.** The base owns every generic panel; a product dashboard
   holds only its product rows and links to the base dashboards with the time
   range carried. Grafana provisions every `dashboards/` directory of the
   search path in place, without staging copies: each is mounted read-only at
   `/hakit/dashboards/<i>`, `i` its entry's position on the path (so a mount
   keeps its name whatever the entries before it hold), and
   `hakit dashboards --compose-override` writes the mounts of the `HAKIT_PATH`
   entries. The provider reads them recursively into one folder, and
   `hakit validate` reads the same set. Every dashboard carries the generic
   block: window regions from `sip_chaos_window == 1` and phase points from
   `sip_chaos_event`, read from the VictoriaMetrics datasource (uid `vm`) and
   titled by incident and phase, the tag `hakit`, and a link listing every
   dashboard so tagged with the time range kept. `hakit validate` refuses a
   dashboard on the path without it, and a dashboard uid or title defined
   twice. When a verdict is written the runner pushes it as series for the
   dashboards (`sip_chaos_window_verdict`, `sip_chaos_window_findings`,
   `sip_chaos_run_verdict`); a failed push is said and noted in the summary,
   and the verdict stands.
6. **Series labels.** Every series a generator exposes and every record carry
   `stream` and `shape`, so the generic panels show a product's shapes
   without product panels.
7. **Metric catalogue.** A product extends the contract through `metrics/`:
   catalogue files in the `metric-contract export` JSON format plus a
   top-level `namespaces`, the name prefixes the product owns. The name
   checker reads the base's export plus every `metrics/` file of the search
   path (`metric-contract check --content <entry>`) and checks every name a
   reader spells under a declared prefix, so a product with one family still
   has its other names refused. It refuses a catalogue without declared
   prefixes, a family under none of them, and a family the contract already
   holds. The runner's own source is a reader too. The series the tooling
   itself pushes (`sip_chaos_*`, and the `sip_soak_*` state a workflow
   supervising a run pushes) and the SIPp exporter twin's record counters are
   declared in the base's `content/metrics/`, not allow-listed; loadgen's
   record and not-placed counters are in its own catalogue.

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

# The HA tooling is a pluggable base

**Status:** accepted (2026-10-15)

**Owner:** `tooling/hakit/` (the runner, its schemas in `tooling/hakit/schemas/`,
its own content in `tooling/hakit/content/`), `deploy/helm/siprust/` and
`deploy/helm/siprust-observability/` (the charts), `crates/loadgen` (the
outcome records it writes), `crates/metric-catalogue` and
`crates/metric-contract` (the catalogue a product extends).

## Context

The endurance and chaos tooling grew as shell around the lab:
`deploy/k8s/chaos.sh` (853 lines) and `endurance.sh` (1637) hold every fault
primitive, the load streams, the gates and the tolerances in one place, and a
product that builds on this repository reuses them by sourcing `chaos.sh` and
overriding functions. Four things went wrong with that shape.

- **The damage was judged per script, after the fact.** Each event pushes
  `sip_chaos_event{type,phase|result}` and is gated by its own rule (a pass
  threshold, an excused class). Nothing says what impact an event may cause,
  so a gate cannot tell an expected residual from a defect, and failures
  between events are counted by no gate.
- **The judge was blind to single calls.** SIPp reports counters only
  (`deploy/k8s/sipp/exporter/`), so a call a fault stranded, a call left
  without a BYE when its generator was removed, or a burst of 20 calls under a
  rate rule could not be attributed to anything. loadgen classes its calls
  (`crates/loadgen/src/class.rs`) and excuses some of them as `near` a fault
  (`chaos.rs`), a second acceptance mechanism beside the gates.
- **Variants were made by rewriting files.** Scenario variants are separate
  XML copies, or are patched with `sed` at launch; manifests are rendered by
  `envsubst`, which blanked a shell variable in the proxy's init script. A
  product forks the manifests and the dashboards it needs to change.
- **There was no second product in view.** Every seam was cut for the one
  downstream product that existed, by overriding shell functions. A product
  that adds media handling, with its own faults and its own impact classes,
  would have to fork again.

## Decision

The HA tooling is one pluggable base: a Python runner that discovers incidents,
impact budgets, run profiles, scenarios, dashboards, treatment catalogs and
metric catalogues on a search path, runs a profile against the lab, and judges
every call by a versioned outcome record. A product plugs in by adding
directories to the search path. It never forks a file of the base.

### 1. The search path

1. `HAKIT_PATH` is a colon-separated list of directories. The search path is
   `tooling/hakit/content/` followed by the `HAKIT_PATH` entries in order.
2. Each entry may hold:

   | directory | holds | schema |
   |---|---|---|
   | `incidents/<name>/` | `incident.yaml`, executables `inject`, `heal`, optional `probe` | `incident.v1` |
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
   that fails is refused with the path and the schema error.
5. Data is the contract and code is the leaf. Where an incident or a class
   needs logic, its directory carries an executable in any language; the
   runner knows it only through the contract below.
6. Every schema is versioned in its name (`call-outcome.v1`). Any change to a
   schema is a new major version file beside the old one. The runner reads
   the versions it ships and refuses the others.

### 2. The incident contract

An incident says what happens and to what. It never says what impact is
tolerated (§3).

1. **Layout.** `incidents/<name>/incident.yaml` (fields in
   `incident.v1.schema.json`: `kind` `fault|load`, `target` selector and pick,
   `params`, `duration_s`, `timeouts`, `requires`) and the executables
   `inject`, `heal` and, optionally, `probe`.
2. **Phases.** For each event the runner records, in order: `inject` (before
   it runs `inject`), `injected` (`inject` exited 0), `heal` (after
   `duration_s`, before it runs `heal`), `healed` (`heal` exited 0),
   `recovered` (`probe` exited 0, or right after `healed` without a probe).
   An executable that fails or overruns its timeout records `failed` and an
   `incident_failed` integrity finding. A combined event injects its
   incidents in the listed order and heals them in reverse; each incident
   records its own phases.
3. **Environment.** Each executable runs with its working directory set to
   the event directory and receives:

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
   else, or the timeout (the runner kills the process group), fails the event.
6. **Heal always runs.** The runner calls `heal` after a failed `inject`, on
   its own interruption, and from `hakit cleanup` for every event a crashed
   run left open. So `heal` is idempotent and correct after a partial
   `inject` or none.
7. **Backends.** The fault mechanism is the executable's choice: the shell
   primitives of `pod-faults.sh` (node stop, cgroup freeze, netem delay and
   loss, TCP reset), a Chaos Mesh resource it creates and deletes, `kubectl`.
   An incident that needs a backend names it in `requires`, and the runner
   refuses the profile before the run if the lab lacks it.
8. **Load incidents.** An incident of `kind: load` starts a load stream at
   `inject` and stops it at `heal`. Stopping is clean: the generator stops
   placing calls, the calls in flight finish their teardown, and only then is
   the generator removed (SIPp: `SIGUSR1`, then a wait of hold + ring + margin;
   loadgen: rate 0, then its drain). A load incident never kills its generator
   with calls in flight. The stream's records carry its own `stream` name, so
   its impact is budgeted per stream like any other.
9. **The event series.** At each phase the runner pushes
   `sip_chaos_event{incident,phase,source,run} 1` to VictoriaMetrics with the
   phase instant as its timestamp. `source` is `schedule` (fired by the
   profile), `manual` (`hakit fire`) or `detected` (an involuntary event the
   runner saw, such as a generator that died). While a window is open the
   runner also pushes `sip_chaos_window{incident,run}` at 1 every 10 s and 0
   when it closes, so a range query at any step sees the window. These series
   are for display. The verdict reads the run's own `events.jsonl`, which
   holds the same instants.

### 3. The impact-budget contract

A budget says what impact a window allows. It never says how an incident is
made. A run profile joins the two.

1. **Impact classes.** The classifier derives these from outcome records (§4),
   each with an instant:

   | class | from the record | instant | budget |
   |---|---|---|---|
   | `setup_failed` | an ending before answer that the catalog does not expect for the shape (`rejected` with another code, `no_final`, `abandoned` not expected), keyed by stream × `final_code` (0 for none) × `reason` | `setup_ended_at_ms` | per key |
   | `setup_slow` | `setup_ended_at_ms − started_at_ms` above the threshold | `setup_ended_at_ms` | count |
   | `dialog_request_failed` | each `failures` entry of kind `dialog_request` | its `at_ms` | 0 |
   | `call_cut` | `ending: cut` | the failure that cut it, else `ended_at_ms` | 0 |
   | `call_stranded` | `ending: stranded` | `ended_at_ms` | 0 |
   | `protocol_anomaly` | each `failures` entry of kind `protocol` or `transport`; a leg whose ending its catalog leg does not expect | its `at_ms` | 0 |
   | `cdr_mismatch` | an answered call with no CDR, a CDR with no record, or a CDR whose duration or final differs; joined on `call_id` | `ended_at_ms` | count |
   | `recovery` | seconds from `healed` until traffic is back to baseline (§3.5) | `healed` | seconds |
   | `unattributed` | an impact of any class above whose instant is outside every window | its instant | 0, never budgeted |

   One record may yield several impacts (a re-INVITE answered 481 is a
   `dialog_request_failed`, and the call it ended is a `call_cut`). A record
   whose `failures` hold a `generator` entry is not an impact of the system
   under test: it is a `generator_failure` integrity finding.

2. **Nothing is accepted by default.** A class or key a budget does not name
   allows 0. A `setup_failed` code a budget does not name is a finding: the
   code is never a wildcard, because a new failure code is exactly what a run
   exists to show. The in-dialog classes (`dialog_request_failed`,
   `call_cut`, `call_stranded`, `protocol_anomaly`) take no budget at all: an
   established call is what HA protects. The verdict counts them per window
   and keeps every record for analysis.
3. **Keys.** A `setup_failed` entry names `stream` (or `*`, each stream
   separately), `code`, `reason` (or `*`) and either `max_calls` or
   `max_seconds`. `max_seconds` is the allowance in seconds of the stream's
   offered rate, so one budget holds at any cps. A failed setup is charged to
   the first entry that matches it.
4. **Windows.** An event's window opens at its `inject` instant and closes
   `settle_s` after its `recovered` instant. An impact belongs to the window
   that holds its instant; one outside every window is `unattributed`. The
   windows of a profile never overlap: `hakit run --dry-run` computes them
   with every timeout at its maximum and refuses a profile where two could.
   Incidents that must act together are one event (`incidents: [...]`) with
   one window and one budget. `settle_s` is at least the longest setup
   deadline of the streams, so a call stranded at the fault reaches its
   deadline inside the window.
5. **Recovery.** The baseline of each stream is its successful-setup rate and
   its p99 setup time over `baseline_s` before `inject`, read from the
   generators' series in VictoriaMetrics. Traffic is back when, for `hold_s`
   in a row, every stream's rate is within `tolerance` below its baseline and
   its p99 within `tolerance` above. `recovery_s` is the time from `healed`
   to the start of that span; a budget's `recovery.max_s` caps it, and
   without one traffic must be back before the window closes.
6. **Added classes.** A product adds a class with `classes/<name>.yaml` and
   its executable `classes/<name>.count`, which reads the run directory and
   prints the impacts with their instants. Attribution and budgets work on it
   as on a built-in class (`budget.classes.<name>.max`).
7. **The verdict.** `verdict.json` (`verdict.v1`): per stream the calls, the
   records and the successes; per window the phases, the facts, every impact
   with its count, its seconds of offered rate, its allowance and whether it
   is a finding; the unattributed impact; the integrity findings. `status` is
   `invalid` when an integrity finding exists, else `fail` when any impact is
   a finding, else `pass`. A human `summary.md` is rendered from it.

### 4. The call-outcome record

1. **One record per call.** Every generator writes one `call-outcome.v1`
   record (`tooling/hakit/schemas/call-outcome.v1.schema.json`) when it is
   done with a call, on every exit path: success, failure, timeout, abort.
   The record states the caller's facts (`started_at_ms`,
   `setup_ended_at_ms`, `answered_at_ms`, `ended_at_ms`, `phase_reached`,
   `final_code`, `reason`, `ending`, `failures`, `legs`) and never a
   judgement. The generator's own class goes in `producer_verdict`, which the
   classifier does not read, so the generator cannot excuse a call.
2. **Producers.** SIPp writes the record with its `<log>` action at every exit
   label of every scenario, into a file in the stream's stats directory. The
   exporter twin follows the file, ships each line and counts the records
   (`stream`, `shape`, `ending`). loadgen writes the same record natively for
   each call, its result classes mapped by the table below.
3. **Transport.** A record reaches VictoriaLogs as one line on
   `/insert/jsonline` with `_stream_fields=run,source,stream` and `_time` set
   from `ended_at_ms`. VictoriaLogs is the live view. At the end of a run the
   runner exports the run's records into the run directory, and the verdict
   reads that copy, so a verdict can be recomputed after VictoriaLogs
   retention has dropped the run.
4. **Reconciliation.** For each SIPp stream, the records equal the delta of
   SIPp's own created-calls counter over the run, read after the drain. A
   difference is a `record_gap` integrity finding: a scenario exit that writes
   no record would otherwise hide exactly the calls that went wrong.
5. **The classifier** is one module of the runner,
   `tooling/hakit/src/hakit/classify.py`: a pure function of a record and its
   catalog shape to impacts with instants. Neither generators nor dashboards
   classify.
6. **loadgen's classes.** A record's `ending` comes from what the caller
   observed, never from an accepted branch's name:

   | loadgen class | record |
   |---|---|
   | `ok` | the observed ending (`answered`, `rejected` with its code, `abandoned`); the branch in `producer_verdict` |
   | `status_<code>` | before answer `rejected` with `final_code`; after answer a `dialog_request` failure with `code` |
   | `timeout` | before answer `no_final`; after answer a `dialog_request` failure without `code`, or `stranded` on the BYE |
   | `wrong_method`, `unexpected`, `unparseable`, `rfc_audit_fail`, `check_fail` | a `protocol` failure, `ending: failed` if it stopped the call |
   | `transport` | a `transport` failure, `ending: failed` |
   | `rejected`, `panic` | a `generator` failure, `ending: failed` |

   loadgen's `near`/`clear` tag and its `POST /chaos` marker play no part in
   the verdict: attribution is the window's (§3.4).

### 5. The treatment catalog

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

### 6. The runner

1. **Language.** Python, the package `hakit` at `tooling/hakit/` (pyproject
   and a uv lockfile; the only dependencies are PyYAML and a JSON Schema
   validator, pinned). pyright strict and pytest run in the verify gates.
2. **Subcommands.** `hakit validate`, `hakit up [--profile P]` (the cluster
   and the charts, §7), `hakit run <profile>`, `hakit fire <incident>` (one
   manual event), `hakit cleanup` (heal every open event), `hakit verdict
   <run-dir>`. `--dry-run` applies to all of them.
3. **Injected clock and executor.** Every wait goes through a clock and every
   effect (an executable, `kubectl`, `helm`, `docker`, an HTTP push or query)
   through an executor. `--dry-run` runs the whole profile on a fake clock and
   a fake executor that records each call and returns scripted results, and
   prints the timeline with every window. A test runs every profile on the
   search path that way in seconds.
4. **The run directory.** `$HAKIT_RESULTS/<run>/`, the run id
   `<profile>-<UTC yyyymmddThhmmssZ>`: the resolved profile, `run-meta.json`
   (the tree that ran, the host facts), `events.jsonl`, `events/<event>/`,
   the exported records, `verdict.json`, `summary.md`. The run id is also the
   nonce the generators put in their Call-IDs, so two runs never share one.
5. **Before the load.** The runner records the host (free memory, swap, disk,
   load) and refuses the run outside the profile's `preflight` limits; it
   refuses to start while the system under test holds calls (a
   `leftover_calls` finding) or while an event of an earlier run is open.
6. **During the load.** Every series the verdict reads is checked for
   freshness; one that stops updating for three scrape intervals is a
   `series_stale` integrity finding with its instant. A generator that exits
   before the load ends is `stream_died`.
7. **The fault primitives stay shell** behind the incident contract. The
   readers that judge a soak today move into the package as they are, and are
   wired to the verdict where they overlap.

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
   `sip_chaos_window`) and the generators' record counters are declared in
   the base's `content/metrics/`, not allow-listed.

## Considered and rejected

- **Sourcing a shell library and overriding functions.** It is how a product
  plugs in today, and it couples the product to every function name and
  global of the base. A second product would inherit the same coupling.
- **Plug-in code (Python entry points) instead of directories.** A product
  would have to write Python against the runner's internals for what is data
  in almost every case. Executables cover the remainder in any language.
- **Tolerances inside the incident.** Today's gates hold their thresholds
  beside the fault. The same kill then needs a copy per tolerance, and two
  products cannot share a fault while disagreeing on what it may cost.
- **Judging from counters.** Counters cannot place a call in a window or join
  it with a CDR, and a burst of a few calls hides under a rate rule.
- **A Rust runner.** The runner shells out, waits and parses YAML; the work is
  orchestration. Python with pyright strict and a fake executor gives the
  same refusal of untested paths at a fraction of the build cost, and the
  readers it absorbs are Python already.
- **A deterministic in-process incident engine first.** Real kills, freezes
  and partitions on the lab find what the in-process harness cannot model.
  An incident that exposed a real defect is later reproduced in the harness
  through the same incident spec.

## Consequences

- `chaos.sh`, the endurance scripts, `sipext-gen.sh`'s stream launcher where
  the runner replaces it, the raw manifests and their `envsubst` rendering go
  once the runner and the charts cover them. No fix lands in the old tooling
  meanwhile.
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
- Every SIPp scenario gains a record at every exit, and the exporter twin
  gains the record shipping and counter; a scenario without a record on some
  exit shows as a `record_gap`.
- A product's dashboards, metric families, incidents, budgets and profiles
  live in its own repository and reach the base only through `HAKIT_PATH`.

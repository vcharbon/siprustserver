# The HA tooling is a pluggable base

**Status:** accepted (2026-10-15)

**Owner:** `tooling/hakit/` (the runner, its schemas and content),
`deploy/helm/siprust/` and `deploy/helm/siprust-observability/` (the charts),
`crates/loadgen` (the product-shape seam and its outcome records),
`crates/metric-catalogue` and `crates/metric-contract` (the catalogue a
product extends).

Tables, field lists and the SIPp engine facts are in the [appendix](appendix/0049-the-ha-tooling-is-a-pluggable-base.md), under the
same section and item numbers.

## Context

Fault primitives, load streams, gates and tolerances held in one shell
library extend only by sourcing it and overriding functions. That shape
judges each event by its own rule after the fact, so an expected residual
cannot be told from a defect and failures between events count nowhere. It
reads counters, so a single failed call cannot be attributed. A second product with its own faults, load
shapes and classes would couple to every function name of the base.

## Decision

The HA tooling is one pluggable base: a Python runner that discovers incidents,
impact budgets, run profiles, scenarios, dashboards, treatment catalogs and
metric catalogues on a search path, runs a profile against the lab, and judges
every call by a versioned outcome record. A product plugs in by adding
directories to the search path. It never forks a file of the base. Every seam
is cut for a second product whose code the base does not know.

### 1. The search path

The search path is `tooling/hakit/content/` followed by the `HAKIT_PATH`
entries. Each entry holds `incidents/`, `budgets/`, `profiles/`, `catalogs/`,
`classes/`, `metrics/`, `scenarios/` and `dashboards/`. A name defined twice
on the path is refused, naming both files. A product changes a base incident
through its profile's parameters and targets, never by shadowing it. Every
file is validated against its versioned schema before anything runs. Data is
the contract and an executable in any language is the leaf.
Items 1.1–1.6: [appendix §1](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#1-the-search-path).

### 2. The incident contract

An incident says what happens and to what, never what impact is tolerated
(§3). A fault has executables `inject`, `heal` and optionally `probe`, run
with a fixed `HAKIT_*` environment; a load incident has a stream the runner
itself starts at `inject` and drains or abandons at `heal`. The runner records
the phases `inject`, `injected`, `heal`, `healed`, `recovered` (or `failed`).
`heal` always runs and is idempotent. A step concludes only from a positive
answer. A heal that fails twice makes the run `invalid`. The base incidents
target pods by label, so a product points each at its own components.
Items 2.1–2.11: [appendix §2](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#2-the-incident-contract).

### 3. The impact-budget contract

A budget says what impact a window allows, never how an incident is made. A
run profile joins the two. The classifier derives impact classes from outcome
records and catalog shapes. A class or key a budget does not name allows 0,
and a failure code is never a wildcard. In-dialog impacts take no budget by
default. Each event opens a window from its first `inject` to `settle_s`
after recovery; an impact belongs to the window holding its instant, else it
is `unattributed`. Recovery, CDR mismatches, added classes, generator stalls,
limiter self-repair, established calls and one-at-a-time faults have stated
rules. `verdict.json` is `invalid` on an integrity finding, else `fail`
on any finding impact, else `pass`.
Items 3.1–3.15: [appendix §3](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#3-the-impact-budget-contract).

### 4. The call-outcome record

Every caller-side generator writes one `call-outcome.v1` record per call
placed, on every exit path. It states the caller's facts and never a
judgement; `producer_verdict` is not read by the classifier, so a generator
cannot excuse a call. SIPp UAC scenarios follow lint-checked rules so every
path writes a record. loadgen maps its result classes to the record. Records
reach VictoriaLogs in one shape, and the verdict reads the run directory's
export. Records plus calls not placed plus calls the generator failed equal
the generator's offered-call counter, else a `record_gap`. The classifier is
the one pure module `tooling/hakit/src/hakit/classify.py`.
Items 4.1–4.6: [appendix §4](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#4-the-call-outcome-record).

### 5. The treatment catalog and the product-shape seam

A treatment catalog maps each shape id to its request, the callee legs it
opens, the caller endings that count as success and whether it is
`load_safe`. The runner, the SIPp renderer, loadgen and the classifier read
the same catalog, so a request and its judged ending cannot drift. loadgen is a library taking a
`ShapeRegistry`; a product registers its shapes under catalog ids. A stream's
generator runs as a docker container or, for an unreachable system under
test, as a pod.
Items 5.1–5.5: [appendix §5](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#5-the-treatment-catalog-and-the-product-shape-seam).

### 6. The runner

The runner is the Python package `hakit` (pyright strict, pytest).
Its subcommands are `validate`, `up`, `run`, `fire`, `cleanup`, `verdict`,
`pcap`, `mark` and the readers. Every wait goes through an injected clock and
every effect through an executor, so `--dry-run` runs any profile in
seconds. A run directory holds everything a verdict recomputes from. The
runner refuses a run on a stressed host or one with leftover calls, samples
host pressure every second, checks live series for freshness and applies rate
steps. Fault primitives stay shell; product terms of the readers are flags or
a `--spec` file.
Items 6.1–6.7: [appendix §6](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#6-the-runner).

### 7. Deployment, dashboards and metrics

`deploy/helm/siprust/` deploys the system under test and its generic
backends; a product depends on it from its own umbrella chart and never
copies a manifest. `deploy/helm/siprust-observability/` deploys the
in-cluster collectors. VictoriaMetrics, VictoriaLogs and Grafana stay in the
host compose stack. `hakit up` passes the lab values; nothing is rendered by
`envsubst`. Every dashboard on the path carries the generic window block. A
product extends the metric contract through `metrics/` with declared
prefixes.
Items 7.1–7.7: [appendix §7](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#7-deployment-dashboards-and-metrics).

## Considered and rejected

Long form: [appendix](appendix/0049-the-ha-tooling-is-a-pluggable-base.md#considered-and-rejected-long-form).

- Sourcing a shell library and overriding functions: couples a consumer to every base name.
- Python entry points instead of directories: code against runner internals for data.
- Tolerances inside the incident: one copy of a fault per tolerance.
- Judging from counters: cannot place a call in a window or join it with a CDR.
- An incident executable that starts its own load: its stream escapes every runner check.
- A Rust runner: orchestration work at a higher build cost.
- An in-process incident engine first: misses what real faults find.

## Consequences

- The base's SIPp scenarios and dashboards live in
  `tooling/hakit/content/`, so the base is the first search-path entry like
  any other.
- SIPp variants use SIPp-native parameters (`-key`, `-d`, injection files); a
  renderer exists only for attribute-level variants, and nothing rewrites a
  scenario with `sed` or `perl`. The bench reads the same scenarios.
- Every SIPp UAC scenario writes a record at every exit and passes the lint
  of §4.2; the exporter twin ships and counts the records.
- A product's dashboards, metric families, incidents, budgets and profiles
  live in its own repository and reach the base only through `HAKIT_PATH`.

# HA tooling vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "HA tooling vocabulary".

The language of the pluggable HA tooling base
([ADR-0049](../../docs/adr/0049-the-ha-tooling-is-a-pluggable-base.md)): the
runner `hakit`, what it discovers on the search path, and how it judges a run.

**Search path**:
`tooling/hakit/content/` followed by the directories of `HAKIT_PATH`. Each
entry may hold incidents, budgets, profiles, treatment catalogs, impact
classes, metric catalogues, scenarios and dashboards; a name defined twice on
the path is refused.
_Avoid_: "plug-in" for the code of a product (a product adds directories, not
code to the runner); overriding a base file by placing one of the same name
earlier.

**Incident**:
A named thing that happens to the lab, in `incidents/<name>/`: what it acts
on, `inject`, `heal` and an optional `probe`. A *fault* incident's
executables act on a deployed component; a *load incident* names a profile
stream the runner starts at inject and drains at heal. An incident never
states a tolerance.
_Avoid_: "chaos event" for the definition (an **event** is one firing of an
incident in a run); a pass threshold beside the fault.

**Window** (incident window):
The span an event's budgetable impact is charged to: from its `inject`
instant to `settle_s` after its `recovered` instant. Windows of one run never
overlap. An in-dialog impact names its *candidate windows* instead: those
whose inject fell inside the call's life before the failure.
_Avoid_: "gate" (a window is a span impact is charged to, not a check after an event).

**Impact class**:
What went wrong with a call or a run, derived from outcome records and the
treatment catalog: `setup_failed`, `unexpected_ending`, `setup_slow`,
`dialog_request_failed`, `call_cut`, `call_stranded`, `protocol_anomaly`,
`cdr_mismatch`, `recovery`, `unattributed`, plus the classes a product adds.
_Avoid_: loadgen's result class (`timeout`, `status_503`), which is what the
generator saw, not what the system under test did.

**Impact budget**:
What impact one window allows, per class, `setup_failed` and a pre-answer
`unexpected_ending` keyed by stream × final code × reason. A class or key the
budget does not name allows 0; the in-dialog classes take a budget only
through `in_dialog`, for a fault that outlasts their transactions' timeout
or reorders a dialog's requests, and an unexpected ending after answer takes none; impact outside every window is
`unattributed` and always a finding.
_Avoid_: "tolerance" for something an incident or a scenario carries.

**Run profile**:
One run in `profiles/<name>.yaml`: the load streams, the schedule of incident
events and the budget of each event's window.
_Avoid_: **Load profile** (loadgen's own run spec for one load fleet).

**Load stream**:
One named flow of calls of one shape at one rate, from one generator, for the
whole run or only inside a load incident's event; its name is the `stream` of
its records and the `stream` label of its series.
_Avoid_: "role" (a SIPp exporter label, not a stream).

**Call-outcome record**:
The one record a caller-side generator writes per call it placed, on every
exit path, stating what the caller observed (`call-outcome.v1`). It carries the generator's own class
only as `producer_verdict`, which the classifier never reads.
_Avoid_: a generator classifying or excusing its own calls.

**Treatment catalog**:
The shapes a system under test offers, by shape id: how a caller requests the
treatment, the callee-side legs it opens and the endings that count as
success (`treatment-catalog.v1`). The base's own catalog requests treatments
with `X-Api-Call`.
_Avoid_: an accepted ending stated anywhere but the catalog.

**Verdict**:
`verdict.json` of a run: per window its impact against its budget, the
unattributed impact and the integrity findings; `pass`, `fail`, or `invalid`
when the measurement itself is in doubt (a record gap, a failed incident, a
stale series).

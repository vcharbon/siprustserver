# E2E checks as post-call audit over canonical anchors

## Status

accepted

## Context

A **Test case** must assert things like "Bob1's received INVITE From URI
user-part matches `\+33…`", verify PAI / R-URI / Diversion, and reach into the SDP
body. Read naively, "Bob verifies the From" suggests Bob evaluating assertions
*live* as it receives. We also want these identity checks authored **once** and
reused across many shapes, and the same verdict whether the Infra shape is fake or
real.

## Decision

Checks are **declarative, evaluated post-call over the recorded trace by the same
engine as the 77 RFC audit rules** — a check is a *parameterised audit rule
contributed by the JSON*, surfaced as a verdict/anomaly in the report exactly like
an RFC finding. Bob does **not** evaluate anything live; checks cannot influence
the flow.

A check keys off `<agent>.<anchor>`, where the **anchor** is a name from a
**canonical, project-wide vocabulary** (`initialInvite`, `reInvite`,
`firstProvisional`, `answer`, `ack`, `bye`, `refer`, …) that each Callflow shape
*publishes* for the messages it produces. Anchors are **per-agent**, so the
rerouted INVITE is `bob2.initialInvite`. Binding by anchor (not step index) makes
a **Check set** — a committed, reusable bundle of checks — portable across every
shape that publishes the anchors it references; an unpublished referenced anchor
is a load-time compatibility error.

Field grammar: **URI-bearing headers** (From/To/PAI/PPI/R-URI/Diversion[]/Contact[])
expose typed helpers `.userInfo/.host/.port/.displayName/.tag/.param(x)`; **any
other header** gets `.present/.absent/.regex`; the **payload** gets `.body.regex`
(SDP-aware helpers later); and the **transport source/dest** (`source.ip/.port`,
`dest.ip/.port`, from the recorded `from`/`to` `SocketAddr`) is assertable so a
Test case may verify which IP a message came from (e.g. b-leg source == LB VIP).
This is an opt-in **capability** — there is deliberately **no** mandatory/default
source-IP check baked into any Infra shape. Values may bind a Test-case input (`${input.from}`) so
"the SUT preserved/rewrote From" is one shared, parameterised check. A selector
that matches no recorded message **fails loudly** unless marked optional. The
spectral media classifier verdict ("alice hears bob") is just another check.

## Considered options

- **Live, Bob-side assertions during the flow.** Could fail-fast and gate the flow
  (reject if From doesn't match). Rejected: imperative, bifurcates fake/real
  timing, and doesn't reuse the audit engine. The post-call model keeps fake and
  real byte-identical and renders into the existing SVG/anomaly report for free.
- **Per-shape free-string anchors.** Rejected: cross-shape sharing then depends on
  authors coincidentally agreeing on names; no load-time compatibility guarantee.
- **Raw `{method, nth}` structural selectors.** Rejected as the surface form: less
  readable and fragile when a shape changes.

## Consequences

- A check can only assert over what was *recorded*; it cannot make a UA behave
  differently. Flow-gating behaviour, if ever needed, must live in the Callflow
  shape (Rust), not in a check.
- The canonical anchor vocabulary is a shared contract: adding a common anchor is a
  deliberate, project-wide act, and every shape must map its messages onto it.

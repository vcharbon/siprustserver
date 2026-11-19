# Delta rules run on a generic engine

**Status:** accepted (2026-11-16), amended (2026-11-18).

**Owner:** `ts/delta-rules/` (the rule type, the engine, the readings, the lane lists, the
overlap graph, the rules page), `ts/rfc-delta-rules/` (the generic rows, their order and the
facts they read), `ts/bless/` (the post-run bless). The readings, the stack-owned values and
a worked placement: [appendix](appendix/0062-delta-rules-run-on-a-generic-engine.md).

## Context

A replayed cell is confronted with its reference capture. Every difference is a probe
(`@sip/pipeline` `Probe`), and the `Classifier` seam asks the deployment which named rule the
probe is and what the lane's lists make of that rule. To write rules, a deployment needs a rule
type, a match order, readings of a probe, lane lists, a rendering of its rules, and a post-run
bless. None of these belong to one deployment. Some differences arise on every B2BUA replay of
every capture, because the stack allocates its own values; others where a captured side breaks
a named RFC clause, or from what this repository's B2BUA and replay rig do by design.

A delta rule is not an exemption (ADR-0001): an exemption accepts a non-RFC shape at the
ingress gate; a delta rule names a difference between a replay and its reference.

## Decision

1. **A rule** (`rule.ts`) has an `id`, a match key `on: {kind, family?, scope?}`, an optional
   `gate` and `rfc`, a `why` of at most 300 characters, lane notes, and a predicate over one
   probe and the deployment's case facts. The engine is generic over the facts type and the
   section names. The builder tests the key before the predicate, so a rule never matches a
   probe outside its key. Rules that differ by one parameter form a family table
   (`families.ts`) whose rows name their ids.
2. **Order is first match, by section, then by position.** The deployment states its sections,
   their order and the place of every rule; `matchRule` takes the first, `matchingRules` lists
   every rule that names the probe. `shapeIssues` refuses a section split or out of order, an id
   minted twice and a `why` or lane note over its cap; a deployment's test asserts it empty. The
   overlap graph (`overlap.ts`) reads full match sets over the deployment's fixtures, their
   mutants and any archive it keeps, and lists each pair of rules seen naming one probe, with
   the pair's winner. The deployment checks that list in as a golden file and a test
   regenerates it: an edit that changes a winner fails, naming the pair. A `why` never explains
   position.
3. **The readings a predicate is written with** are the engine's (appendix). A reading that
   names a deployment's header vocabulary, mode or leg roles stays with the deployment.
4. **What a deployment supplies:**
   - its case-facts type, read once per cell;
   - its sections and rows, where each generic row sits (item 6), and that row's lane notes;
   - its lane lists. An entry is a rule id classed `accepted`, or `known_bug` with the ticket
     it waits on. A list may extend another, the nearest list wins, and a list beside a lane's
     own files stands in for the shipped one of its name. The loader refuses
     (`DeltaRules.LaneList*`) an unknown rule, an `extends` cycle, a rule in both classes of one
     file, a `known_bug` entry with no ticket and an `accepted` one with a ticket;
   - any classification ahead of the rules, such as a header namespace it injects itself;
   - the intro, section titles and lane order the rules page prints, and any table of its own
     the page closes with;
   - the sink the bless audit files into (`@sip/bless/BlessAudit/Sink`).
5. **The generic rows** (`@sip/rfc-delta-rules`) name a difference in RFC or 3GPP terms, of
   one of three kinds:
   - a value or an outcome an RFC leaves to the sending stack or to arrival order, arising
     on any B2BUA replay of any capture (appendix);
   - a break of a named RFC clause by the captured side, which the replayed system may keep or
     reproduce;
   - the documented behaviour of this repository's B2BUA or replay rig, citing in place of a
     clause the ADR that states it (the rig's is ADR-0060).

   The one exception: a third-party stack's behaviour that breaks no named clause is a row only
   where a reference-pack capture needs it. A row moves here once it is generic. No row
   here names a break by the replayed system itself: a deployment states those. Each row cites
   its clause or ADR in `why` and `rfc` and carries no lane note; its fixtures follow the
   appendix. A fact one row reads lives beside it; a fact moves to the engine only when an RFC
   states it and two rows here read it. A header list lives beside its rows and cites its RFC
   or 3GPP clause, or is named a grouping.
6. **Upstream orders its rows.** `@sip/rfc-delta-rules` states its sections, their order and
   each row's place, and pins each winner among its rows with an overlap golden file over its
   fixtures and their mutants. A deployment may put a row of its own ahead of, between or after
   them; its order restricted to the generic rows equals upstream's, so no deployment changes
   the winner between two generic rows.
7. **Ids never change.** Lane lists, stored verdicts and rule-hit counts read them. The one
   exception is a row that moves here with an id in a deployment's vocabulary: it takes a
   generic id, and the deployment renames it in every reader (lane lists, golden files, stored
   verdicts) in one commit.
8. **The bless is a `Reclassifier`** (`@sip/bless`).
   - A failed cell passes when every refused datagram, absence and omitted 100 is a shape
     record a named rule answers at its own step, and every record is `accepted` or
     `known-bug`.
   - An RFC charge the system only relayed from a peer drops to recorded.
   - A body row no rule accepts fails the cell.
   - It audits a passing cell and files each finding against its rule, never the cell.
   - A cell file that will not read is logged and read as absent: the verdict stands.

## Considered and rejected

- **A priority per row, or upstream sections placed whole.** A deployment's row may win over a
  generic row on some probes and lose to it on others, and may need to sit between two generic
  rows of one section (appendix).
- **The generic rows as one block ahead of the deployment's.** Some deployment rows (an
  exchange the stack answers itself, a dialog's own address) win over generic rows. A block
  would move those winners.
- **No two generic rows matching one probe.** A captured side's break and a value the stack
  allocates can sit on one header, so disjoint keys would keep one of them out.
- **A row moving here once a second deployment consumes it.** A generic row kept in one
  deployment is written again by the next.
- **The engine in `@sip/pipeline`.** The pipeline extracts and confronts; writing rules is a
  deployment's concern built on top of it. The `Classifier` and `Reclassifier` seams stay there.
- **The bless inside `@sip/delta-rules`.** It reads a finished cell's records and files, never
  a rule: a separate concern.
- **Header vocabulary in the engine.** Which headers a deployment groups as a family is its own
  reading of the platform it replays.

## Consequences

- `@sip/pipeline` keeps the seams and their neutral layers. `@sip/delta-rules` and `@sip/bless`
  depend on it and on `@sip/contracts`; `@sip/rfc-delta-rules` depends on `@sip/delta-rules`.
- A deployment binds the engine once to its facts and sections, and its own readings import the
  engine's.
- A deployment places each generic row by name, adding lane notes only. One of its tests holds
  that every generic row is placed exactly once, in upstream's order, and runs the rows'
  fixtures (`@sip/rfc-delta-rules/test-fixtures`) through the placed rules. Its case facts take
  the rows' facts whole from `rfcFactsOf`.
- A new generic row has no effect until a deployment places it; until then its probes keep
  their rulings.

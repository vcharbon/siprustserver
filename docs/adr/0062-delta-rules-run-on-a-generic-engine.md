# Delta rules run on a generic engine

**Status:** accepted (2026-11-16).

**Owner:** `ts/delta-rules/` (the rule type, the engine, the readings, the lane lists, the
overlap graph, the rules page), `ts/rfc-delta-rules/` (the rows an RFC alone justifies),
`ts/bless/` (the post-run bless).

## Context

A replayed cell is confronted with its reference capture. Every difference is a probe
(`@sip/pipeline` `Probe`), and the `Classifier` seam asks the deployment which named rule the
probe is and what the lane's lists make of that rule. To write rules, a deployment needs a rule
type, a match order, readings of a probe, lane lists, a rendering of its rules, and a post-run
bless that restates a cell when a named rule answers every failure. None of these belong to one
deployment. Some differences also arise on every B2BUA replay of every capture: the stack
allocates its own Via branch, Call-ID, tags, CSeq and RSeq, and recomputes Content-Length.

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
   every rule that names the probe. The overlap graph (`overlap.ts`) reads full match sets over
   the deployment's fixtures, their mutants and any archive it keeps, and lists each pair of
   rules seen naming one probe, with the pair's winner. The deployment checks that list in as a
   golden file and a test regenerates it: an edit that changes a winner fails, naming the pair.
   A `why` never explains position.
3. **The readings a predicate is written with** are the engine's: a header's items and their
   set delta, their provenance, the message a probe rides, name-addr identity (RFC 3261
   §19.1.4), value forms, status sets, the run facts of the probe's leg, and the cell's document
   reading (ADR-0061). A reading that names a deployment's header vocabulary, mode or leg roles
   stays with the deployment.
4. **What a deployment supplies:**
   - its case-facts type, read once per cell;
   - its sections and rows, where each RFC row sits, and that row's lane notes;
   - its lane lists. An entry is a rule id with a class: `accepted`, or `known_bug` with the
     ticket it waits on. A list may extend another, the nearest list wins, and a list beside a
     lane's own files stands in for the shipped one of its name. The loader refuses a list that
     names an unknown rule, a cycle, and an entry in both classes or with the wrong ticket
     shape (`DeltaRules.LaneList*`);
   - any classification ahead of the rules, such as a header namespace it injects itself;
   - the intro, section titles, lane order and refusal rows the rules page prints;
   - the sink the bless audit files into (`@sip/bless/BlessAudit/Sink`).
5. **The RFC rows.** A row belongs here when the difference it names arises on any B2BUA replay
   of any capture, because an RFC makes the value the sending stack's own: Via, Call-ID, the
   CSeq number, the From and To tags, Max-Forwards, RSeq, RAck, Content-Length and the multipart
   boundary. Each row cites its clause in `why` and `rfc`. Its fixtures use 555 numbers and
   RFC 5737 addresses. The rows carry no section and no lane note. No two of them match one
   probe, so they impose no order among themselves. A fact that one row reads lives beside that
   row. A fact moves to the engine only when an RFC states it and two rows here read it.
6. **Ids never change.** Lane lists, stored verdicts and rule-hit counts read them.
7. **The bless is a `Reclassifier`** (`@sip/bless`).
   - A failed cell passes when two conditions hold. Every refused datagram, absence and
     omitted 100 is a shape record that a named rule answers at its own step. Every record is
     `accepted` or `known-bug`.
   - An RFC charge that the system only relayed from a peer drops to recorded.
   - A body row that no rule accepts fails the cell.
   - The bless audits a passing cell and files each finding against its rule, never the cell.
   - A cell file that will not read is logged and read as absent: the verdict stands.

## Considered and rejected

- **Rows carrying a section or a priority.** A deployment's row may win over an RFC row on some
  probes and lose to it on others. Only the deployment can place the rows.
- **The RFC rows as one block ahead of the deployment's.** Some deployment rows (an exchange the
  stack answers itself, a dialog's own address) win over RFC rows. A block would move those
  winners.
- **The engine in `@sip/pipeline`.** The pipeline extracts and confronts. Writing rules is a
  deployment's concern built on top of it. The `Classifier` and `Reclassifier` seams stay in the
  pipeline.
- **The bless inside `@sip/delta-rules`.** The bless reads a finished cell's records and files,
  never a rule: it is a separate concern.
- **Header vocabulary in the engine.** Which headers a deployment groups as a family is its own
  reading of the platform it replays.

## Consequences

- `@sip/pipeline` keeps the seams and their neutral layers. `@sip/delta-rules` and `@sip/bless`
  depend on it and on `@sip/contracts`; `@sip/rfc-delta-rules` depends on `@sip/delta-rules`.
- A deployment binds the engine once to its facts and sections, and its own readings import the
  engine's.
- A deployment's registry imports each RFC row by name and places it. One of the deployment's
  tests holds that every RFC row is placed exactly once.
- A deployment must place a new RFC row before it has any effect. Until then, that row's
  probes keep the rulings they had.
- More rows move here when a second deployment consumes them.

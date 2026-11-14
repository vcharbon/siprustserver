/**
 * Call facts, document side (ADR-0061): what a case document states of its
 * legs and calls, read off its steps and its `calls` block.
 *
 * A document states what a replay will do, not what a capture showed. The cut
 * drops keepalives and stray ACKs, folds a retransmission into the step it
 * repeats, adds far-side re-INVITE steps and reorders relayed provisionals: an
 * entry's place is its place in the replay, its time is where the capture
 * stamped the message the step was cut from, and its in-dialog marker is the
 * document's own (`in_dialog`: after its leg's dialog-creating 2xx). A step
 * holds no CSeq number, so an INVITE and its final are paired by position
 * (`./transactions.ts`). The two sides agree on a named subset ({@link Agreed})
 * and on nothing else.
 *
 * The scripted actor is the party: a `send` step is the party's message, an
 * `expect` step is the system's.
 */
import { type Call, Flow, type Flows, Pivot } from "@sip/contracts"
import type { CutVantage } from "./cut.js"
import { atVantage, type Fact, type LegFacts, ofVantage, valueOf } from "./leg-facts.js"
import type { SutSet } from "./sut.js"
import { inviteTransactions } from "./transactions.js"

const ABSENT: Fact<never> = { state: "absent" }
const stated = <T>(value: T | undefined): Fact<T> => (value === undefined ? ABSENT : { state: "seen", value })

/** One message step of a document. */
export interface StepEntry {
  /** The source pointer: the step itself. */
  readonly step: Flow.Step
  /** Its place among the document's message steps, in document order. */
  readonly index: number
  /** Whether the system sends it; else the party does. */
  readonly bySystem: boolean
  /** A request's method, a response's CSeq method; upper-case. */
  readonly method: string
  /** A response's status; `undefined` on a request. */
  readonly status: number | undefined
  /** Whether it rides its leg's confirmed dialog. */
  readonly inDialog: boolean
  /** How many repeats the step declares. */
  readonly retransmissions: number
  /** When the message the step was cut from was captured, where the document says. */
  readonly at_us: number | undefined
}

/** How a leg ends in the document: the BYE or the CANCEL step. */
export interface DocumentEnding {
  readonly by: "BYE" | "CANCEL"
  readonly entry: StepEntry
}

/**
 * What a document states of one leg. A fact is `seen` where a step states it
 * and `absent` otherwise: a document has no edge to lose a message at.
 */
export interface DocumentLeg {
  readonly leg: string
  /** The leg's message steps, in document order. */
  readonly entries: ReadonlyArray<StepEntry>
  /** The INVITE that opens the leg: its first dialog-opening one. */
  readonly invite: Fact<StepEntry>
  /** The first 180–189 answering the leg's last dialog-opening INVITE. */
  readonly ringing: Fact<StepEntry>
  /** The final the leg holds for its last dialog-opening INVITE. */
  readonly final: Fact<StepEntry>
  /** That final, where it is a 2xx. */
  readonly answer: Fact<StepEntry>
  /**
   * How the leg ends: an answered leg on its first BYE after the answer, an
   * unanswered one on the CANCEL of its last dialog-opening INVITE, else on a
   * BYE of its early dialog.
   */
  readonly ending: Fact<DocumentEnding>
}

/** One attempt of a call's chain as its `calls` entry states it, with its leg. */
export interface DocumentAttempt {
  /** The entry itself: its final, its cause and what joined it are read here. */
  readonly attempt: Call.Attempt
  readonly leg: DocumentLeg | undefined
}

/** What a document states of one call. */
export interface DocumentCall {
  readonly call: Call.Call
  readonly caller: DocumentLeg | undefined
  readonly attempts: ReadonlyArray<DocumentAttempt>
  /** The failure final the routing decision refused the caller with, where the call states one. */
  readonly refused: StepEntry | undefined
  /** The caller's CANCEL that abandoned the call before any dial, where the call states one. */
  readonly abandoned: StepEntry | undefined
}

/** What a document states, over all its legs and calls. */
export interface DocumentFacts {
  readonly pivot: Pivot.PivotV3
  /** Every message step, in document order. */
  readonly entries: ReadonlyArray<StepEntry>
  readonly legs: ReadonlyMap<string, DocumentLeg>
  readonly calls: ReadonlyArray<DocumentCall>
  /** The entry of the step `id` names. */
  readonly entryOf: (id: string) => StepEntry | undefined
}

const entryOf = (step: Flow.Step, index: number): StepEntry => ({
  step,
  index,
  bySystem: step.op === "expect",
  method: step.msg.status === undefined ? Flow.stepMethod(step) : Flow.stepCseqMethod(step),
  status: step.msg.status,
  inDialog: step.in_dialog === true,
  retransmissions: step.retransmits ?? 0,
  at_us: step.observed?.at_us
})

const isRinging = (e: StepEntry): boolean =>
  e.method === "INVITE" && e.status !== undefined && e.status >= 180 && e.status < 190 && !e.inDialog

const silentLeg = (leg: string): DocumentLeg => ({
  leg,
  entries: [],
  invite: ABSENT,
  ringing: ABSENT,
  final: ABSENT,
  answer: ABSENT,
  ending: ABSENT
})

const legOf = (
  leg: string,
  entries: ReadonlyArray<StepEntry>,
  byId: ReadonlyMap<string, StepEntry>,
  transactions: ReadonlyArray<{ readonly invite?: Flow.Step; readonly final?: { readonly step: Flow.Step } }>
): DocumentLeg => {
  const openings = transactions.filter((t) => t.invite !== undefined && Flow.opensDialog(t.invite))
  const outcome = openings[openings.length - 1]
  const first = openings[0]?.invite
  if (outcome?.invite === undefined || first === undefined) return { ...silentLeg(leg), entries }
  const invite = byId.get(outcome.invite.id)!
  const final = outcome.final === undefined ? undefined : byId.get(outcome.final.step.id)
  const answered = final !== undefined && (final.status ?? 0) < 300
  const after = entries.filter((e) => e.index > invite.index)
  const ringing = after.find((e) => isRinging(e) && e.bySystem !== invite.bySystem && (final === undefined || e.index < final.index))
  const bye = after.find((e) => e.method === "BYE" && e.status === undefined && (!answered || e.index > final.index))
  const cancel = after.find((e) => e.method === "CANCEL" && e.status === undefined && e.bySystem === invite.bySystem)
  const ending: DocumentEnding | undefined = answered
    ? bye && { by: "BYE", entry: bye }
    : cancel !== undefined
      ? { by: "CANCEL", entry: cancel }
      : bye && { by: "BYE", entry: bye }
  return {
    leg,
    entries,
    invite: stated(byId.get(first.id)),
    ringing: stated(ringing),
    final: stated(final),
    answer: stated(answered ? final : undefined),
    ending: stated(ending)
  }
}

const documents = new WeakMap<Pivot.PivotV3, DocumentFacts>()

/**
 * The facts of a case document, computed on the first request in one pass over
 * its steps and kept for the document: a second request reads nothing.
 */
export const ofDocument = (pivot: Pivot.PivotV3): DocumentFacts => {
  const known = documents.get(pivot)
  if (known !== undefined) return known
  const steps = Pivot.pivotSteps(pivot)
  const entries = steps.map(entryOf)
  const byId = new Map(entries.map((entry) => [entry.step.id, entry]))
  const byLeg = new Map<string, Array<StepEntry>>()
  for (const entry of entries) {
    const held = byLeg.get(entry.step.leg)
    if (held === undefined) byLeg.set(entry.step.leg, [entry])
    else held.push(entry)
  }
  const transactions = inviteTransactions(steps).byLeg
  const legs = new Map<string, DocumentLeg>()
  for (const [leg, held] of byLeg) legs.set(leg, legOf(leg, held, byId, transactions.get(leg) ?? []))
  const facts: DocumentFacts = {
    pivot,
    entries,
    legs,
    calls: pivot.calls.map((call) => ({
      call,
      caller: legs.get(call.caller_leg),
      attempts: call.attempts.map((attempt) => ({ attempt, leg: legs.get(attempt.leg) })),
      refused: call.refused === undefined ? undefined : byId.get(call.refused.step),
      abandoned: call.abandoned === undefined ? undefined : byId.get(call.abandoned.step)
    })),
    entryOf: (id) => byId.get(id)
  }
  documents.set(pivot, facts)
  return facts
}

/**
 * What the capture side and the document side both state of a leg, and agree
 * on (ADR-0061): who sent the opening INVITE, the status of the first 18x, the
 * status of the outcome final, whether the leg was answered, how it ended and
 * who ended it. A capture's `not-captured` and a document's silence both read
 * as `undefined` here.
 */
export interface Agreed {
  readonly invite: "system" | "party" | undefined
  readonly ringing: number | undefined
  readonly final: number | undefined
  readonly answered: boolean
  readonly ending: { readonly by: "BYE" | "CANCEL"; readonly bySystem: boolean } | undefined
}

const sideOf = (entry: { readonly bySystem: boolean } | undefined): Agreed["invite"] =>
  entry === undefined ? undefined : entry.bySystem ? "system" : "party"

const endingOf = (
  ending: { readonly by: "BYE" | "CANCEL"; readonly entry: { readonly bySystem: boolean } } | undefined
): Agreed["ending"] => (ending === undefined ? undefined : { by: ending.by, bySystem: ending.entry.bySystem })

/** The agreed subset as a capture's leg facts state it. */
export const agreedOfCapture = (leg: LegFacts): Agreed => ({
  invite: sideOf(valueOf(leg.invite)),
  ringing: valueOf(leg.ringing)?.status,
  final: valueOf(leg.final)?.status,
  answered: leg.answer.state === "seen",
  ending: endingOf(valueOf(leg.ending))
})

/** The agreed subset as a document's leg states it. */
export const agreedOfDocument = (leg: DocumentLeg): Agreed => ({
  invite: sideOf(valueOf(leg.invite)),
  ringing: valueOf(leg.ringing)?.status,
  final: valueOf(leg.final)?.status,
  answered: leg.answer.state === "seen",
  ending: endingOf(valueOf(leg.ending))
})

/** One leg of a case on which its document and its capture state the agreed subset differently. */
export interface Disagreement {
  /** The document's leg. */
  readonly leg: string
  /** The vantage the leg was cut at. */
  readonly vantage: CutVantage
  readonly document: Agreed
  readonly capture: Agreed
}

const sameSubset = (a: Agreed, b: Agreed): boolean =>
  a.invite === b.invite &&
  a.ringing === b.ringing &&
  a.final === b.final &&
  a.answered === b.answered &&
  a.ending?.by === b.ending?.by &&
  a.ending?.bySystem === b.ending?.bySystem

/** A case document held against the capture it was cut from, on the agreed subset. */
export interface Parity {
  /** How many of the document's legs were held against a vantage. */
  readonly compared: number
  /** The legs, among those, that state the subset differently from the capture. */
  readonly disagreements: ReadonlyArray<Disagreement>
}

/**
 * A case document held against the capture it was cut from. A document leg is
 * held against the vantage, among those the case was cut at, that owns the
 * captured message its first observed step names; a leg no step of which was
 * observed, or whose message no vantage owns, is held against nothing and is
 * not counted in `compared`.
 */
export const parity = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>,
  pivot: Pivot.PivotV3
): Parity => {
  let compared = 0
  const disagreements = [...ofDocument(pivot).legs.values()].flatMap((leg): ReadonlyArray<Disagreement> => {
    const observed = leg.entries.find((e) => e.step.observed !== undefined)?.step.observed
    if (observed === undefined) return []
    const vantage = vantages.find((v) => v.leg === observed.leg && ofVantage(flows, sut, v).msgIdxs.includes(observed.msg))
    if (vantage === undefined) return []
    compared += 1
    const document = agreedOfDocument(leg)
    const capture = agreedOfCapture(atVantage(flows, sut, vantage))
    return sameSubset(document, capture) ? [] : [{ leg: leg.leg, vantage, document, capture }]
  })
  return { compared, disagreements }
}

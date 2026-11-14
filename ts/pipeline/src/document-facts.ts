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
import { type Call, Flow, type Flows, type Pivot } from "@sip/contracts"
import { type NoLater, releaseIn, type ReleaseOf } from "./call-of-legs.js"
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
  /** The capture leg the document's leg was cut from: the one its first observed step names. */
  readonly observedLeg: number | undefined
  /** The INVITE that opens the leg: its first dialog-opening one. */
  readonly invite: Fact<StepEntry>
  /** The INVITE that decides the leg: its last dialog-opening one. */
  readonly outcome: Fact<StepEntry>
  /** The first 180–189 answering the outcome INVITE. */
  readonly ringing: Fact<StepEntry>
  /** The final the leg holds for the outcome INVITE. */
  readonly final: Fact<StepEntry>
  /** That final, where it is a 2xx. */
  readonly answer: Fact<StepEntry>
  /** The CANCEL of the outcome INVITE, sent by the side that sent that INVITE (RFC 3261 §9.1). */
  readonly cancel: Fact<StepEntry>
  /** Every BYE request after the outcome INVITE, in document order. */
  readonly byes: ReadonlyArray<StepEntry>
  /**
   * How the leg ends: an answered leg on its first BYE after the answer, an
   * unanswered one on the CANCEL of its outcome INVITE, else on a BYE of its
   * early dialog.
   */
  readonly ending: Fact<DocumentEnding>
  /**
   * The party left the leg: its CANCEL of the outcome INVITE (RFC 3261 §9.1)
   * or its first BYE after that INVITE (§15), the earlier in document order.
   */
  readonly left: Fact<DocumentEnding>
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
  /** The caller was answered: the 2xx of the caller's leg. */
  readonly answered: Fact<StepEntry>
  /** The caller got a failure final: the final of its outcome INVITE, where it is 300 or above. */
  readonly failed: Fact<StepEntry>
  /** The caller gave up: the party left the caller's leg ({@link DocumentLeg.left}). */
  readonly gaveUp: Fact<DocumentEnding>
  /** The failure final the routing decision refused the caller with, where the call states one. */
  readonly refused: StepEntry | undefined
  /** The caller's CANCEL that abandoned the call before any dial, where the call states one. */
  readonly abandoned: StepEntry | undefined
}

/** What the facts are read off: a document's flow and its `calls` block. */
export type Stated = Pick<Pivot.PivotV3, "flow" | "calls">

/**
 * What a document's message steps state of their legs, read before or without
 * its `calls` block: the reading a case's flow has as soon as it is assembled.
 */
export interface StepFacts {
  /** Every message step, in document order. */
  readonly entries: ReadonlyArray<StepEntry>
  readonly legs: ReadonlyMap<string, DocumentLeg>
  /** The entry of the step `id` names. */
  readonly entryOf: (id: string) => StepEntry | undefined
  /**
   * Whether an ACK step discharges the INVITE transaction the final step `id`
   * landed on while that transaction held it (RFC 3261 §13.2.2.4 and §17.1.1.3,
   * paired by position as `./transactions.ts` pairs them).
   */
  readonly acked: (id: string) => boolean
}

/** What a document states, over all its legs and calls. */
export interface DocumentFacts<D extends Stated = Pivot.PivotV3> extends StepFacts {
  /** The document read. */
  readonly pivot: D
  readonly calls: ReadonlyArray<DocumentCall>
  /** What the document states of the call `call` is, among its `calls`. */
  readonly callOf: (call: Call.Call) => DocumentCall | undefined
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

type Unleft = Omit<DocumentLeg, "left">

const silentLeg = (leg: string, entries: ReadonlyArray<StepEntry>): Unleft => ({
  leg,
  entries,
  observedLeg: entries.find((e) => e.step.observed !== undefined)?.step.observed?.leg,
  invite: ABSENT,
  outcome: ABSENT,
  ringing: ABSENT,
  final: ABSENT,
  answer: ABSENT,
  cancel: ABSENT,
  byes: entries.filter(isByeRequest),
  ending: ABSENT
})

const withLeft = (leg: Unleft): DocumentLeg => ({ ...leg, left: leftOf(leg) })

const leftOf = (leg: Unleft): Fact<DocumentEnding> => {
  const cancel = valueOf(leg.cancel)
  const bye = leg.byes.find((b) => !b.bySystem)
  const left: ReadonlyArray<DocumentEnding> = [
    ...(cancel !== undefined && !cancel.bySystem ? [{ by: "CANCEL", entry: cancel } as const] : []),
    ...(bye !== undefined ? [{ by: "BYE", entry: bye } as const] : [])
  ]
  return stated(left.reduce<DocumentEnding | undefined>((first, next) => (first === undefined || next.entry.index < first.entry.index ? next : first), undefined))
}

const isByeRequest = (e: StepEntry): boolean => e.method === "BYE" && e.status === undefined

const legOf = (
  leg: string,
  entries: ReadonlyArray<StepEntry>,
  byId: ReadonlyMap<string, StepEntry>,
  transactions: ReadonlyArray<{ readonly invite?: Flow.Step; readonly final?: { readonly step: Flow.Step } }>
): Unleft => {
  const openings = transactions.filter((t) => t.invite !== undefined && Flow.opensDialog(t.invite))
  const outcome = openings[openings.length - 1]
  const first = openings[0]?.invite
  if (outcome?.invite === undefined || first === undefined) return silentLeg(leg, entries)
  const invite = byId.get(outcome.invite.id)!
  const final = outcome.final === undefined ? undefined : byId.get(outcome.final.step.id)
  const answered = final !== undefined && (final.status ?? 0) < 300
  const after = entries.filter((e) => e.index > invite.index)
  const ringing = after.find((e) => isRinging(e) && e.bySystem !== invite.bySystem && (final === undefined || e.index < final.index))
  const byes = after.filter(isByeRequest)
  const bye = byes.find((e) => !answered || e.index > final.index)
  const cancel = after.find((e) => e.method === "CANCEL" && e.status === undefined && e.bySystem === invite.bySystem)
  const ending: DocumentEnding | undefined = answered
    ? bye && { by: "BYE", entry: bye }
    : cancel !== undefined
      ? { by: "CANCEL", entry: cancel }
      : bye && { by: "BYE", entry: bye }
  return {
    ...silentLeg(leg, entries),
    invite: stated(byId.get(first.id)),
    outcome: stated(invite),
    ringing: stated(ringing),
    final: stated(final),
    answer: stated(answered ? final : undefined),
    cancel: stated(cancel),
    byes,
    ending: stated(ending)
  }
}

const failureOf = (leg: DocumentLeg | undefined): Fact<StepEntry> => {
  const final = leg === undefined ? undefined : valueOf(leg.final)
  return stated(final !== undefined && (final.status ?? 0) >= 300 ? final : undefined)
}

const callOf = (call: Call.Call, { entryOf, legs }: StepFacts): DocumentCall => {
  const caller = legs.get(call.caller_leg)
  return {
    call,
    caller,
    attempts: call.attempts.map((attempt) => ({ attempt, leg: legs.get(attempt.leg) })),
    answered: caller === undefined ? ABSENT : caller.answer,
    failed: failureOf(caller),
    gaveUp: caller === undefined ? ABSENT : caller.left,
    refused: call.refused === undefined ? undefined : entryOf(call.refused.step),
    abandoned: call.abandoned === undefined ? undefined : entryOf(call.abandoned.step)
  }
}

const stepLists = new WeakMap<ReadonlyArray<Flow.Step>, StepFacts>()

/**
 * The facts of a document's message steps, computed on the first request in
 * one pass over them and kept for that array: a second request reads nothing.
 */
export const ofSteps = (steps: ReadonlyArray<Flow.Step>): StepFacts => {
  const known = stepLists.get(steps)
  if (known !== undefined) return known
  const entries = steps.map(entryOf)
  const byId = new Map(entries.map((entry) => [entry.step.id, entry]))
  const byLeg = new Map<string, Array<StepEntry>>()
  for (const entry of entries) {
    const held = byLeg.get(entry.step.leg)
    if (held === undefined) byLeg.set(entry.step.leg, [entry])
    else held.push(entry)
  }
  const transactions = inviteTransactions(steps)
  const legs = new Map<string, DocumentLeg>()
  for (const [leg, held] of byLeg) legs.set(leg, withLeft(legOf(leg, held, byId, transactions.byLeg.get(leg) ?? [])))
  const acked = new Set(
    [...transactions.discharges.values()].flatMap((d) => (transactions.landings.get(d.final.id) === d.transaction ? [d.final.id] : []))
  )
  const facts: StepFacts = { entries, legs, entryOf: (id) => byId.get(id), acked: (id) => acked.has(id) }
  stepLists.set(steps, facts)
  return facts
}

/** A flow's message steps: the flow itself where it holds nothing else, so its reading is the one {@link ofSteps} keeps. */
const stepsOfFlow = (flow: ReadonlyArray<Flow.FlowNode>): ReadonlyArray<Flow.Step> =>
  flow.every(Flow.isStepNode) ? (flow as ReadonlyArray<Flow.Step>) : flow.flatMap(Flow.flowNodeSteps)

const documents = new WeakMap<Stated, DocumentFacts<Stated>>()

/**
 * The facts of a case document, computed on the first request and kept for the
 * document: a second request reads nothing. A document whose flow is its
 * message steps shares their reading ({@link ofSteps}): the steps an assembly
 * read before the document existed are not read again.
 */
export const ofDocument = <D extends Stated>(pivot: D): DocumentFacts<D> => {
  const known = documents.get(pivot)
  if (known !== undefined) return known as DocumentFacts<D>
  const steps = ofSteps(stepsOfFlow(pivot.flow))
  const calls = pivot.calls.map((call) => callOf(call, steps))
  const byCall = new Map(calls.map((held) => [held.call, held]))
  const facts: DocumentFacts<D> = { ...steps, pivot, calls, callOf: (call) => byCall.get(call) }
  documents.set(pivot, facts)
  return facts
}

/** {@link ReleaseOf} as a document states it. */
export type DocumentRelease = ReleaseOf<DocumentEnding, StepEntry>

/**
 * A document's order: `a` is no later than `b` where it stands no later in the
 * document, the order a replay runs; past that, where both steps were cut
 * from a capture, where the capture stamped it within `slackUs` after `b`.
 */
const noLaterStated: NoLater<StepEntry> = (a, b, slackUs) =>
  a.index <= b.index || (slackUs > 0 && a.at_us !== undefined && b.at_us !== undefined && a.at_us <= b.at_us + slackUs)

/**
 * Whether the system releases a leg of `call` on its own, as the document
 * states it: the one definition (`releaseIn`, `./call-of-legs.ts`) read in the
 * document's order. `skewUs` is the clock skew of the capture the document
 * was cut from.
 */
export const release = (call: DocumentCall, leg: DocumentLeg, skewUs: number): DocumentRelease => {
  const gaveUp = valueOf(call.gaveUp)
  return releaseIn(
    {
      ending: valueOf(leg.ending),
      dialled: valueOf(leg.outcome),
      answered: valueOf(call.answered),
      giveUps: gaveUp === undefined ? [] : [gaveUp.entry]
    },
    noLaterStated,
    skewUs
  )
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
 * The vantage a document leg was cut at: the one, among those the case was cut
 * at, that owns the captured message the leg's first observed step names.
 * `undefined` for a leg no step of which was observed, or whose message no
 * vantage owns.
 */
export const vantageOf = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>,
  leg: DocumentLeg
): CutVantage | undefined => {
  const observed = leg.entries.find((e) => e.step.observed !== undefined)?.step.observed
  if (observed === undefined) return undefined
  return vantages.find((v) => v.leg === observed.leg && ofVantage(flows, sut, v).msgIdxs.includes(observed.msg))
}

/**
 * A case document held against the capture it was cut from. A document leg is
 * held against the vantage it was cut at ({@link vantageOf}); a leg cut at
 * none is held against nothing and is not counted in `compared`.
 */
export const parity = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  vantages: ReadonlyArray<CutVantage>,
  pivot: Pivot.PivotV3
): Parity => {
  let compared = 0
  const disagreements = [...ofDocument(pivot).legs.values()].flatMap((leg): ReadonlyArray<Disagreement> => {
    const vantage = vantageOf(flows, sut, vantages, leg)
    if (vantage === undefined) return []
    compared += 1
    const document = agreedOfDocument(leg)
    const capture = agreedOfCapture(atVantage(flows, sut, vantage))
    return sameSubset(document, capture) ? [] : [{ leg: leg.leg, vantage, document, capture }]
  })
  return { compared, disagreements }
}

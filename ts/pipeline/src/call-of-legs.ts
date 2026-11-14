/**
 * Call facts over legs: what needs more than one leg to state (ADR-0061).
 *
 * The caller of this module says which legs form the call: the caller's own
 * legs, and the chain, the legs the system dialled in order, per branch.
 * Nothing here infers either. Every fact is read off the legs' own facts, so
 * a call costs no new pass over its messages.
 */
import { type Ending, type Entry, type Fact, type LegFacts, valueOf } from "./leg-facts.js"

/** The legs of one call, as the caller of the module states them. */
export interface CallLegs {
  /** The caller's legs: those whose dialog-opening INVITE arrived at the system. */
  readonly caller: ReadonlyArray<LegFacts>
  /** The legs the system dialled, in order, per branch. */
  readonly chain: ReadonlyArray<ReadonlyArray<LegFacts>>
}

/** The caller leaving the call: its CANCEL or its BYE, on the leg that shows it. */
export interface GiveUp {
  readonly by: "BYE" | "CANCEL"
  readonly entry: Entry
  readonly leg: LegFacts
}

/** What a call shows over its caller's legs. */
export interface OfCall {
  readonly legs: CallLegs
  /** The caller was answered: the earliest 2xx that answered a caller leg. */
  readonly answered: Fact<Entry>
  /** The caller got a failure final: the earliest one that ended a caller leg. */
  readonly failed: Fact<Entry>
  /**
   * The caller gave up: its earliest CANCEL of a dialog-opening INVITE (RFC
   * 3261 §9.1) or BYE (§15), whichever leg shows it and whether or not the
   * system's own BYE crossed it.
   */
  readonly gaveUp: Fact<GiveUp>
}

const ABSENT: Fact<never> = { state: "absent" }
const NOT_CAPTURED: Fact<never> = { state: "not-captured" }

/** The earliest seen value; else not captured where any candidate is, else absent. */
const earliest = <T>(facts: ReadonlyArray<Fact<T>>, at: (value: T) => number): Fact<T> => {
  let best: T | undefined
  let notCaptured = false
  for (const fact of facts) {
    if (fact.state === "not-captured") notCaptured = true
    if (fact.state !== "seen") continue
    if (best === undefined || at(fact.value) < at(best)) best = fact.value
  }
  if (best !== undefined) return { state: "seen", value: best }
  return notCaptured ? NOT_CAPTURED : ABSENT
}

const failureOf = (leg: LegFacts): Fact<Entry> => {
  if (leg.final.state !== "seen") return leg.final
  return (leg.final.value.status ?? 0) >= 300 ? leg.final : ABSENT
}

/** The party's own leaving of one leg; not captured where the leg's ending is. */
const giveUpOf = (leg: LegFacts): Fact<GiveUp> => {
  const cancels = leg.openings.flatMap((t): ReadonlyArray<GiveUp> =>
    t.cancel !== undefined && !t.cancel.bySystem ? [{ by: "CANCEL", entry: t.cancel, leg }] : []
  )
  const byes = leg.byes.flatMap((bye): ReadonlyArray<GiveUp> => (bye.bySystem ? [] : [{ by: "BYE", entry: bye, leg }]))
  const first = [...cancels, ...byes].reduce<GiveUp | undefined>(
    (best, next) => (best === undefined || next.entry.at_us < best.entry.at_us ? next : best),
    undefined
  )
  if (first !== undefined) return { state: "seen", value: first }
  return leg.ending.state === "not-captured" ? NOT_CAPTURED : ABSENT
}

/** The facts of the call these legs form. */
export const ofCall = (legs: CallLegs): OfCall => ({
  legs,
  answered: earliest(legs.caller.map((leg) => leg.answer), (entry) => entry.at_us),
  failed: earliest(legs.caller.map(failureOf), (entry) => entry.at_us),
  gaveUp: earliest(legs.caller.map(giveUpOf), (giveUp) => giveUp.entry.at_us)
})

/** How long one attempt rang, to the microsecond. */
export interface RingTime {
  /** What ended the ringing: the CANCEL that gave the attempt up, else the final that settled it. */
  readonly end: Fact<Entry>
  /** From the attempt's INVITE to that end. */
  readonly fromInvite_us: Fact<number>
  /** From the attempt's first 180–189 to that end; absent where none preceded it. */
  readonly fromRinging_us: Fact<number>
}

/**
 * One attempt's ring time, measured on its outcome transaction: from its
 * INVITE and from its first 180–189, to the CANCEL that gave it up or the
 * final that settled it.
 */
export const ringTime = (leg: LegFacts): RingTime => {
  const outcome = leg.outcome
  if (outcome?.request === undefined) {
    return { end: leg.final, fromInvite_us: missing(leg.final), fromRinging_us: missing(leg.final) }
  }
  const end = outcome.cancel ?? outcome.final
  if (end === undefined) {
    return { end: leg.final, fromInvite_us: missing(leg.final), fromRinging_us: missing(leg.final) }
  }
  const rang = valueOf(leg.ringing)
  return {
    end: { state: "seen", value: end },
    fromInvite_us: { state: "seen", value: end.at_us - outcome.request.at_us },
    fromRinging_us:
      rang !== undefined && rang.at_us <= end.at_us ? { state: "seen", value: end.at_us - rang.at_us } : ABSENT
  }
}

const missing = (fact: Fact<unknown>): Fact<never> => (fact.state === "not-captured" ? NOT_CAPTURED : ABSENT)

/**
 * Whether the system released a leg it dialled on its own.
 *
 * - `own`: the system ended the leg and nothing the caller did explains it:
 *   its own timer or decision.
 * - `relayed`: the system ended the leg, and `because` on a caller leg
 *   explains it: the caller gave up, or the caller was answered while the
 *   attempt ran, which makes the CANCEL a fork loser's (RFC 3261 §16.7 step 10).
 * - `none`: the system did not end the leg; its party did, a final did, or
 *   the capture does not show its ending.
 */
export type Release =
  | { readonly kind: "own"; readonly ending: Ending }
  | {
      readonly kind: "relayed"
      readonly ending: Ending
      readonly cause: "caller-gave-up" | "caller-answered"
      readonly because: Entry
    }
  | { readonly kind: "none" }

/**
 * "The system released on its own", the one definition, over both endings of a
 * dialled leg: its CANCEL before the final, its BYE after the answer.
 *
 * The caller's give-up explains the release where it is stamped no later than
 * `skewUs` after it: a capture can stamp a relayed message before the message
 * it relays, by its clock skew between hops, and a caller that left earlier
 * left the whole call. The caller's answer explains a CANCEL only, where it
 * is stamped between the attempt's INVITE and that CANCEL.
 */
export const release = (call: OfCall, leg: LegFacts, skewUs: number): Release => {
  const ending = valueOf(leg.ending)
  if (ending === undefined || !ending.entry.bySystem) return { kind: "none" }
  const at = ending.entry.at_us
  const answered = valueOf(call.answered)
  const dialled = leg.outcome?.request?.at_us
  if (ending.by === "CANCEL" && answered !== undefined && dialled !== undefined && answered.at_us >= dialled && answered.at_us <= at) {
    return { kind: "relayed", ending, cause: "caller-answered", because: answered }
  }
  const gaveUp = valueOf(call.gaveUp)
  if (gaveUp !== undefined && gaveUp.entry.at_us <= at + skewUs) {
    return { kind: "relayed", ending, cause: "caller-gave-up", because: gaveUp.entry }
  }
  return { kind: "own", ending }
}

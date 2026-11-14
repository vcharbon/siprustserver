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
   * The caller gave up: the earliest of {@link giveUps}, whichever leg shows
   * it and whether or not the system's own BYE crossed it.
   */
  readonly gaveUp: Fact<GiveUp>
  /**
   * Every leaving of the caller the capture shows, leg by leg ({@link leavingsOf}).
   * The CANCEL of an INVITE the caller then sent again left that INVITE, not
   * the call.
   */
  readonly giveUps: ReadonlyArray<GiveUp>
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

/**
 * Each leaving of one leg by its party, by stamp: its CANCEL of the outcome
 * INVITE stamped before that INVITE's first final, since a CANCEL after it
 * cancels nothing (RFC 3261 §9.1), and each of its BYEs after that INVITE (§15).
 */
const leavingsOf = (leg: LegFacts): ReadonlyArray<GiveUp> => {
  const cancel = leg.outcome?.cancel
  const final = leg.outcome?.firstFinal
  const cancelled = cancel !== undefined && !cancel.bySystem && (final === undefined || cancel.at_us < final.at_us)
  return [
    ...(cancelled ? [{ by: "CANCEL", entry: cancel, leg } as const] : []),
    ...leg.byes.filter((b) => !b.bySystem).map((entry) => ({ by: "BYE", entry, leg }) as const)
  ]
}

/** The party's first leaving of one leg; not captured where the leg's ending is. */
const giveUpOf = (leavings: ReadonlyArray<GiveUp>, leg: LegFacts): Fact<GiveUp> => {
  const first = firstOf(leavings)
  if (first !== undefined) return { state: "seen", value: first }
  return leg.ending.state === "not-captured" ? NOT_CAPTURED : ABSENT
}

const firstOf = (giveUps: ReadonlyArray<GiveUp>): GiveUp | undefined =>
  giveUps.reduce<GiveUp | undefined>(
    (best, next) => (best === undefined || next.entry.at_us < best.entry.at_us ? next : best),
    undefined
  )

/** The facts of the call these legs form. */
export const ofCall = (legs: CallLegs): OfCall => {
  const leavings = legs.caller.map(leavingsOf)
  return {
    legs,
    answered: earliest(legs.caller.map((leg) => leg.answer), (entry) => entry.at_us),
    failed: earliest(legs.caller.map(failureOf), (entry) => entry.at_us),
    gaveUp: earliest(
      legs.caller.map((leg, i) => giveUpOf(leavings[i]!, leg)),
      (giveUp) => giveUp.entry.at_us
    ),
    giveUps: leavings.flat()
  }
}

/** How long one attempt rang, to the microsecond. */
export interface RingTime {
  /** What ended the ringing: the CANCEL that gave the attempt up, else the first final it drew. */
  readonly end: Fact<Entry>
  /** From the attempt's INVITE to that end. */
  readonly fromInvite_us: Fact<number>
  /** From the attempt's first 180–189 to that end; absent where none preceded it. */
  readonly fromRinging_us: Fact<number>
}

/**
 * One attempt's ring time, measured on its outcome transaction: from its
 * INVITE and from its first 180–189, to the CANCEL that gave it up or the
 * first final it drew, whatever a fork answered after that.
 */
export const ringTime = (leg: LegFacts): RingTime => {
  const outcome = leg.outcome
  if (outcome?.request === undefined) {
    return { end: leg.final, fromInvite_us: missing(leg.final), fromRinging_us: missing(leg.final) }
  }
  const end = outcome.cancel ?? outcome.firstFinal
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
export type ReleaseOf<N, E> =
  | { readonly kind: "own"; readonly ending: N }
  | {
      readonly kind: "relayed"
      readonly ending: N
      readonly cause: "caller-gave-up" | "caller-answered"
      readonly because: E
    }
  | { readonly kind: "none" }

/** {@link ReleaseOf} as a capture shows it. */
export type Release = ReleaseOf<Ending, Entry>

/**
 * Whether `a` stands no later than `b`, with `slackUs` of clock skew allowed:
 * the order one side of the facts reads its messages in. A capture reads its
 * stamps; a case document reads its own order (`./document-facts.ts`).
 */
export type NoLater<E> = (a: E, b: E, slackUs: number) => boolean

/** What a release is judged on, on either side of the facts. */
export interface Released<N, E> {
  /** How the leg ended. */
  readonly ending: N | undefined
  /** The INVITE that dialled the leg. */
  readonly dialled: E | undefined
  /** The 2xx that answered the caller. */
  readonly answered: E | undefined
  /** Each give-up of the caller. */
  readonly giveUps: ReadonlyArray<E>
}

/**
 * "The system released on its own", the one definition, over both endings of a
 * dialled leg: its CANCEL before the final, its BYE after the answer.
 *
 * A give-up of the caller explains the release where it stands from `skewUs`
 * before the leg's INVITE to `skewUs` after the release: a capture can stamp a
 * relayed message before the message it relays, by its clock skew between
 * hops. An older give-up left a part of the call this leg was not dialled
 * for. The caller's answer explains a CANCEL only, where it stands between
 * the attempt's INVITE and that CANCEL. The earliest give-up that explains
 * the release is the one named.
 */
export const releaseIn = <E extends { readonly bySystem: boolean }, N extends { readonly by: "BYE" | "CANCEL"; readonly entry: E }>(
  of: Released<N, E>,
  noLater: NoLater<E>,
  skewUs: number
): ReleaseOf<N, E> => {
  const { answered, dialled, ending } = of
  if (ending === undefined || !ending.entry.bySystem) return { kind: "none" }
  const released = ending.entry
  if (
    ending.by === "CANCEL" &&
    answered !== undefined &&
    dialled !== undefined &&
    noLater(dialled, answered, 0) &&
    noLater(answered, released, 0)
  ) {
    return { kind: "relayed", ending, cause: "caller-answered", because: answered }
  }
  const gaveUp = of.giveUps
    .filter((g) => (dialled === undefined || noLater(dialled, g, skewUs)) && noLater(g, released, skewUs))
    .reduce<E | undefined>((best, next) => (best === undefined || !noLater(best, next, 0) ? next : best), undefined)
  if (gaveUp !== undefined) return { kind: "relayed", ending, cause: "caller-gave-up", because: gaveUp }
  return { kind: "own", ending }
}

const noLaterStamped: NoLater<Entry> = (a, b, slackUs) => a.at_us <= b.at_us + slackUs

/** {@link releaseIn} of a capture's leg, every message placed by its stamp. */
export const release = (call: OfCall, leg: LegFacts, skewUs: number): Release =>
  releaseIn(
    {
      ending: valueOf(leg.ending),
      dialled: leg.outcome?.request,
      answered: valueOf(call.answered),
      giveUps: call.giveUps.map((g) => g.entry)
    },
    noLaterStamped,
    skewUs
  )

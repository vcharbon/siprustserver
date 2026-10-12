/**
 * Call bands: which legs belong to which `calls[]` entry, and the stable
 * colour slot that call is drawn in.
 *
 * A concurrency case interleaves several calls through one flow (§4). Without
 * a per-call signal the interleaving reads as one confused sequence, so every
 * call takes a slot: its legs, its arrows and its attempt annotations carry it.
 * The slot is the call's index in `calls[]`, so it never moves between renders.
 */
import type { Call, PivotV3 } from "../generated/pivot-v3.ts"

/** How many distinct colour slots the palette holds before it wraps. */
export const CALL_SLOT_COUNT = 6

export interface AttemptView {
  readonly branch: number
  readonly position: number
  readonly leg: string
  /** The dialed party: the registry's observed value, or its name where the
   * identity is authored and no capture observed one. */
  readonly callee: string
  readonly cause: string | undefined
  /** What ADDED this leg to a running call, where it did not dial out. A joined
   * leg takes its own branch and would otherwise read as a parallel fork. */
  readonly joinedBy: { readonly kind: string; readonly step: string } | undefined
  readonly finalStatus: number | undefined
  readonly finalAtMs: number | undefined
  readonly noAnswerMs: number | undefined
}

export interface CallBand {
  readonly id: string
  /** Position in `calls[]`: the identity the colour slot is derived from. */
  readonly index: number
  /** `index` folded into the palette. */
  readonly slot: number
  readonly callerLeg: string
  /** Caller leg first, then the attempt legs in (branch, position) order. */
  readonly legs: readonly string[]
  readonly attempts: readonly AttemptView[]
  /** The provisional-handling profile the captured system ran, if stated. */
  readonly relay18x: string | undefined
}

export interface CallBands {
  readonly bands: readonly CallBand[]
  /** The call a leg belongs to; the first claimant wins. */
  readonly byLeg: ReadonlyMap<string, CallBand>
  readonly warnings: readonly string[]
}

/** The CSS class carrying a slot's colour. Renderer and legend share it. */
export const callSlotClass = (slot: number): string => `fv-call-${slot % CALL_SLOT_COUNT}`

const attemptViews = (
  call: Call,
  dialled: (name: string) => string
): readonly AttemptView[] =>
  [...call.attempts]
    .sort((left, right) => left.branch - right.branch || left.position - right.position)
    .map((attempt) => ({
      branch: attempt.branch,
      position: attempt.position,
      leg: attempt.leg,
      callee: dialled(attempt.callee.identity),
      cause: attempt.cause ?? undefined,
      joinedBy: attempt.joined_by ?? undefined,
      finalStatus: attempt.final?.status,
      finalAtMs: attempt.final?.at_ms,
      noAnswerMs: attempt.no_answer_ms ?? undefined
    }))

/** Builds one band per declared call, and the leg index the ladder colours by. */
export const callBands = (doc: Pick<PivotV3, "calls" | "identities">): CallBands => {
  const warnings: string[] = []
  const byLeg = new Map<string, CallBand>()
  // An attempt names a registry entry; the number, where one was observed, is
  // the registry's. An authored identity has none, so its name stands in.
  const observed = new Map(doc.identities.map((entry) => [entry.name, entry.observed ?? entry.name]))
  const dialled = (name: string): string => observed.get(name) ?? name
  const bands = doc.calls.map((call, index) => {
    const attempts = attemptViews(call, dialled)
    const legs: string[] = []
    for (const leg of [call.caller_leg, ...attempts.map((attempt) => attempt.leg)]) {
      if (!legs.includes(leg)) legs.push(leg)
    }
    return {
      id: call.id,
      index,
      slot: index % CALL_SLOT_COUNT,
      callerLeg: call.caller_leg,
      legs,
      attempts,
      relay18x: call.relay18x
        ? [call.relay18x.mode, call.relay18x.messages, call.relay18x.prack]
          .filter((token): token is string => typeof token === "string" && token !== "")
          .join(" · ")
        : undefined
    } satisfies CallBand
  })

  for (const band of bands) {
    for (const leg of band.legs) {
      const owner = byLeg.get(leg)
      if (owner === undefined) byLeg.set(leg, band)
      else warnings.push(`leg "${leg}" is claimed by calls ${owner.id} and ${band.id}; drawn as ${owner.id}`)
    }
  }

  return { bands, byLeg, warnings }
}

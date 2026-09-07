/**
 * Declared races: which pairs of steps the capture ordered by nothing a replay
 * can reproduce (§6.1 `overlap`).
 *
 * A capture states the order it saw. It does not always state an order that
 * HOLDS. Where a leg carries a `send` and an ARRIVAL anchored on the same step,
 * the document controls the send's dwell and controls nothing about the
 * arrival: an arrival's instant is the SUT's, relayed (`propagated`) or minted
 * (`sut-originated`) alike, and the two share an anchor precisely when the send
 * interleaves between the arrival and the step that caused it (`./delay.ts`).
 * The captured order is then the difference between a dwell the document holds
 * and a latency it does not, and any SUT quicker or slower than the captured
 * platform by that difference inverts it.
 *
 * Same-leg order is list order and binds, so an undeclared race is a race the
 * document has silently decided. `overlap` is what says otherwise: the two arm
 * together and whichever the wire settles first settles first.
 *
 * The pair must be NEIGHBOURS on their leg — that is what the interpreter can
 * arm together — and the later of the two carries the field.
 *
 * A SECOND shape races for the same reason without a `send` in it: two arrivals
 * on one leg, each answering a different one of our own sends on that leg. The
 * document holds the dwell between the two SENDS and holds nothing about how
 * the SUT interleaves the two independent transactions they opened, so their
 * captured order is only as strong as the captured gap. Where that gap is
 * tighter than the smallest relay this very capture demonstrates, it is below
 * the source platform's own message-moving granularity and no replay
 * reproduces it. A teardown frontier is where this falls out: a request still
 * pending when the BYE lands, and the BYE, answered microseconds apart.
 */
import type { DelayCausality } from "./delay.js"
import type { StepDraft } from "./draft.js"

/**
 * Stamp `overlap` on every same-anchor send/arrival neighbour pair. Mutates
 * `steps`; `derived` is the causality reading parallel to it.
 */
export const stampOverlaps = (
  steps: Array<StepDraft>,
  derived: ReadonlyArray<DelayCausality>
): void => {
  const byLeg = new Map<string, Array<number>>()
  steps.forEach((step, i) => {
    const seen = byLeg.get(step.leg)
    if (seen === undefined) byLeg.set(step.leg, [i])
    else seen.push(i)
  })
  const floor = relayFloorUs(steps, derived)
  for (const positions of byLeg.values()) {
    for (let n = 1; n < positions.length; n++) {
      const earlier = positions[n - 1]!
      const later = positions[n]!
      if (!races(steps, derived, earlier, later) && !arrivalsRace(steps, earlier, later, floor)) {
        continue
      }
      steps[later] = { ...steps[later]!, overlap: steps[earlier]!.id }
    }
  }
}

/** The step an anchor token names, or `undefined` for `trigger`. */
const anchorStep = (from: string): string | undefined =>
  from.startsWith("step:") ? from.slice(5) : undefined

/**
 * The smallest interval this capture demonstrates for moving one message across
 * the SUT: over every `propagated` arrival, its instant minus its origin's.
 * `undefined` where the capture shows no relay at all, which withholds the
 * arrival/arrival stamp rather than guessing a floor.
 */
const relayFloorUs = (
  steps: ReadonlyArray<StepDraft>,
  derived: ReadonlyArray<DelayCausality>
): number | undefined => {
  const at = new Map<string, number>()
  for (const step of steps) {
    if (step.observed !== undefined) at.set(step.id, step.observed.at_us)
  }
  let floor: number | undefined
  steps.forEach((step, i) => {
    if (derived[i] !== "propagated") return
    const origin = anchorStep(step.delay.from)
    const mine = at.get(step.id)
    const theirs = origin === undefined ? undefined : at.get(origin)
    if (mine === undefined || theirs === undefined) return
    const gap = mine - theirs
    if (gap > 0 && (floor === undefined || gap < floor)) floor = gap
  })
  return floor
}

/**
 * Whether two neighbouring arrivals on one leg answer two different sends of
 * OURS on that leg, closer together than the capture's own relay floor.
 *
 * Different anchors is what makes them independent: sharing one is
 * [`races`]'s shape, and the dwell between two different anchors is the
 * document's own — it is the interval between the ANSWERS that the document
 * does not hold.
 */
const arrivalsRace = (
  steps: ReadonlyArray<StepDraft>,
  a: number,
  b: number,
  floorUs: number | undefined
): boolean => {
  if (floorUs === undefined) return false
  if (steps[a]!.op !== "expect" || steps[b]!.op !== "expect") return false
  const from = [steps[a]!.delay.from, steps[b]!.delay.from].map(anchorStep)
  if (from[0] === undefined || from[1] === undefined || from[0] === from[1]) return false
  const causes = from.map((id) => steps.find((s) => s.id === id))
  if (!causes.every((c) => c !== undefined && c.op === "send" && c.leg === steps[a]!.leg)) {
    return false
  }
  const at = [steps[a]!.observed?.at_us, steps[b]!.observed?.at_us]
  if (at[0] === undefined || at[1] === undefined) return false
  return Math.abs(at[1] - at[0]) < floorUs
}

/**
 * Whether two neighbours on one leg are a send and an arrival sharing one
 * anchor — in either order, since which of the two the capture happened to see
 * first is the very thing that does not hold.
 *
 * The shared anchor is what makes the race: measured from two different anchors
 * the two dwells are the document's own, however close together they sit.
 */
const races = (
  steps: ReadonlyArray<StepDraft>,
  derived: ReadonlyArray<DelayCausality>,
  a: number,
  b: number
): boolean => {
  if (steps[a]!.delay.from !== steps[b]!.delay.from) return false
  const [send, arrival] = steps[a]!.op === "send" ? [a, b] : [b, a]
  return (
    steps[send]!.op === "send" &&
    steps[arrival]!.op === "expect" &&
    (derived[arrival] === "propagated" || derived[arrival] === "sut-originated")
  )
}

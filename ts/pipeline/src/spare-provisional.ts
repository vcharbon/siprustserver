/**
 * A caller-facing provisional the SUT is not CAUSED to emit is a tolerated
 * absence, not an obligation (§6.9, issue 106).
 *
 * A captured platform can put more provisionals toward the caller than it took
 * from the callee — it emits one ring twice sub-millisecond apart with a header
 * stripped, or it re-sends its own unreliable 18x on a refresh of its own. Both
 * are transcribed one step each, faithfully, and both then oblige a relaying
 * B2BUA to produce a datagram nothing in the replay causes: RFC 3261 §17.2.1
 * re-sends an unreliable provisional when the INVITE it answers is re-sent, and
 * a replay re-sends nothing.
 *
 * The SURPLUS is what this pass measures — provisional expectations on a leg
 * beyond the peer emissions that anchor them — and `optional` is what it stamps.
 * The step SURVIVES, so its content is still asserted when the SUT does emit it;
 * only the obligation goes.
 *
 * **The surplus is stamped at the END of its run, not on the copy that caused
 * it.** A tolerated absence is released when a LATER step on the same leg
 * matches (§6.5), so it can only be overtaken by a step of a DIFFERENT
 * discriminator — which is the one after the run, never a sibling inside it.
 * Members of a run are interchangeable expectations of one status, so which one
 * carries the tolerance changes nothing about what the leg demands.
 */
import type { StepDraft } from "./draft.js"

/** One expectation the pass made tolerable, for the flag. */
export interface Spared {
  readonly step: string
  readonly leg: string
  readonly status: number
  /** The run it closes, and how many of its members had no anchor of their own. */
  readonly run: number
  readonly surplus: number
}

/**
 * Stamp the surplus of every caller-facing provisional run `optional`, and
 * report what it stamped. Mutates `steps`.
 *
 * Reads `delay`, so it runs after the anchors are assigned.
 */
export const stampSpareProvisionals = (steps: Array<StepDraft>): Array<Spared> => {
  const out: Array<Spared> = []
  for (const run of provisionalRuns(steps)) {
    const surplus = uncausedIn(steps, run)
    // Never the whole run: a leg that may emit NO provisional at all is not
    // what any capture showed, and stamping every member would say exactly that.
    const tolerated = Math.min(surplus, run.length - 1)
    if (tolerated <= 0) continue
    for (const i of run.slice(run.length - tolerated)) {
      steps[i]!.optional = true
      out.push({
        step: steps[i]!.id,
        leg: steps[i]!.leg,
        status: steps[i]!.msg.status!,
        run: run.length,
        surplus: tolerated
      })
    }
  }
  return out
}

/**
 * The maximal runs of consecutive same-status provisional expectations, per leg,
 * as step indices in document order.
 *
 * Relayed and MINTED alike (§6.4's `check`): which of the two a provisional is
 * says where its content came from, and this pass asks what CAUSED it — a
 * question only the anchor answers. A platform that owns the caller-facing
 * provisionals mints them off a peer emission just as a transparent one relays
 * off it; neither mints a second one off nothing.
 */
const provisionalRuns = (steps: ReadonlyArray<StepDraft>): Array<Array<number>> => {
  const runs: Array<Array<number>> = []
  const open = new Map<string, Array<number>>()
  for (let i = 0; i < steps.length; i++) {
    const step = steps[i]!
    if (step.leg === undefined) continue
    const status = provisionalExpected(step)
    const current = open.get(step.leg)
    if (status === undefined) {
      // Any other step on the leg closes its run: what follows it answers to a
      // different cause.
      if (current !== undefined) open.delete(step.leg)
      continue
    }
    if (current !== undefined && steps[current[current.length - 1]!]!.msg.status === status) {
      current.push(i)
      continue
    }
    const fresh = [i]
    runs.push(fresh)
    open.set(step.leg, fresh)
  }
  return runs
}

/** The status this step expects a provisional to carry, if it expects one. */
const provisionalExpected = (step: StepDraft): number | undefined => {
  if (step.op !== "expect") return undefined
  const status = step.msg.status
  // 100 is the stack's own and never relayed; a final is not provisional.
  return status !== undefined && status > 100 && status < 200 ? status : undefined
}

/**
 * How many members of a run have no anchor of their own: their `delay` hangs off
 * an anchor an earlier member already claimed, or off an earlier member itself.
 * Each one is a caller-facing provisional with no peer emission behind it.
 */
const uncausedIn = (steps: ReadonlyArray<StepDraft>, run: ReadonlyArray<number>): number => {
  const claimed = new Set<string>()
  const members = new Set(run.map((i) => steps[i]!.id))
  let uncaused = 0
  for (const i of run) {
    const anchor = steps[i]!.delay.from
    if (claimed.has(anchor) || members.has(anchor.replace(/^step:/, ""))) uncaused++
    else claimed.add(anchor)
  }
  return uncaused
}

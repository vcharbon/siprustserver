/**
 * Every relayed arrival listed after the emission it relays, and a leg's relays
 * in the order of their sources (§6.9).
 *
 * The document lists steps in capture order, and capture order is not always
 * causal order. A B2BUA may relay two provisionals in another order than they
 * reached it, and a vantage that captures the two directions on different
 * interfaces can stamp a relay a little before the datagram it relays. A
 * replaying system relays each source as it arrives, after it arrives: the
 * document expects that.
 *
 * Only a relayed provisional moves — a 101–199 to an INVITE, the one class a
 * relaying platform passes on out of order; an ACK to a non-2xx is hop-by-hop
 * (RFC 3261 §17.1.1.3) and relays nothing. Two moves, both keyed on content
 * (`relayImageOf`), never on time alone, and neither past another step of the
 * arrival's own leg:
 *
 * - **A relay stamped before its source** — an arrival no listed emission
 *   explains, carrying the session description of an emission of the same
 *   message type on another leg stamped at most `RELAY_SKEW_US` later — is
 *   moved to just after that emission. Only a description pairs across that
 *   gap: its origin line names one session version, where a bare message
 *   names nothing a minted one could not equally carry.
 * - **Relays out of their sources' order** — two arrivals of one message type,
 *   adjacent on one leg, whose sources run the other way — swap places.
 *
 * Runs right after the steps are built, before any pass that reads positions.
 * Mutates `steps`, `timings` and `sources` together and renumbers the ids.
 */
import { relayOriginOf, type StepTiming } from "./delay.js"
import type { StepDraft } from "./draft.js"
import { stepId, type StepSource } from "./flowsteps.js"
import { identifiesSession } from "./relay-image.js"

/** Whether a step's type is a provisional to an INVITE above 100: what a relay moves. */
const relayedProvisional = (s: StepTiming): boolean => {
  const m = /^resp:(\d+):INVITE$/.exec(s.typeKey)
  return m !== null && Number(m[1]) > 100 && Number(m[1]) < 200
}

/** Whether a step of `leg` stands strictly between positions `a` and `b`. */
const legBetween = (timings: ReadonlyArray<StepTiming>, leg: string, a: number, b: number): boolean =>
  timings.slice(Math.min(a, b) + 1, Math.max(a, b)).some((t) => t.leg === leg)

/** How much earlier than its source a vantage may stamp a relay (µs). */
export const RELAY_SKEW_US = 500_000

/** One arrival this pass moved, for the flag. */
export interface Placed {
  readonly step: StepDraft
  readonly source: StepDraft
  readonly why: "stamped-before-source" | "out-of-source-order"
}

export interface PlacementInput {
  readonly steps: Array<StepDraft>
  readonly timings: Array<StepTiming>
  readonly sources: Array<StepSource>
}

export const placeRelaysAfterSources = (input: PlacementInput): Array<Placed> => {
  const { steps, timings, sources } = input
  const placed: Array<Placed> = []
  const move = (from: number, to: number): void => {
    for (const arr of [steps, timings, sources] as Array<Array<unknown>>) {
      const [item] = arr.splice(from, 1)
      arr.splice(to, 0, item)
    }
  }

  for (let i = 0; i < steps.length; i++) {
    const s = timings[i]!
    if (s.emits || !relayedProvisional(s) || !identifiesSession(s.image) || relayOriginOf(timings, i) >= 0) {
      continue
    }
    let source = -1
    for (let k = i + 1; k < steps.length; k++) {
      const c = timings[k]!
      if (c.ts_us - s.ts_us > RELAY_SKEW_US) break
      if (c.emits && c.leg !== s.leg && c.typeKey === s.typeKey && c.image === s.image) {
        source = k
        break
      }
    }
    if (source < 0 || legBetween(timings, s.leg, i, source + 1)) continue
    placed.push({ step: steps[i]!, source: steps[source]!, why: "stamped-before-source" })
    // Removing `i` shifts the source down by one: inserting at `source` lands
    // the arrival just after it. The step now at `i` is examined next.
    move(i, source)
    i--
  }

  const origins = timings.map((t, i) => (t.emits || !relayedProvisional(t) ? -1 : relayOriginOf(timings, i)))
  // A run of relayed arrivals of one type adjacent on their leg: nothing of the
  // leg stands between two members, so a swap passes no step of the leg.
  const groups: Array<Array<number>> = []
  const open = new Map<string, Array<number>>()
  timings.forEach((t, i) => {
    const key = `${t.leg}\u0000${t.typeKey}`
    if (origins[i]! >= 0) {
      const run = open.get(t.leg)
      if (run !== undefined && `${t.leg}\u0000${timings[run[0]!]!.typeKey}` === key) run.push(i)
      else {
        const fresh = [i]
        groups.push(fresh)
        open.set(t.leg, fresh)
      }
    } else {
      open.delete(t.leg)
    }
  })
  const order = steps.map((_, i) => i)
  for (const slots of groups) {
    const bySource = [...slots].sort((a, b) => origins[a]! - origins[b]!)
    // The j-th smallest source precedes the j-th slot (each of the first j
    // slots holds an arrival whose source precedes it), so every arrival
    // still follows its source once placed.
    bySource.forEach((arrival, j) => {
      if (arrival === slots[j]) return
      order[slots[j]!] = arrival
      placed.push({ step: steps[arrival]!, source: steps[origins[arrival]!]!, why: "out-of-source-order" })
    })
  }
  if (order.some((from, to) => from !== to)) {
    const [s, t, o] = [[...steps], [...timings], [...sources]]
    order.forEach((from, to) => {
      steps[to] = s[from]!
      timings[to] = t[from]!
      sources[to] = o[from]!
    })
  }

  if (placed.length > 0) {
    steps.forEach((step, i) => {
      step.id = stepId(i + 1)
    })
    sources.forEach((source, i) => {
      sources[i] = { ...source, id: stepId(i + 1) }
    })
  }
  return placed
}

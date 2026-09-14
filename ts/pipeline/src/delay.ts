/**
 * Delay anchoring and `measured` / `propagated` / `sut-originated` causality
 * classification. Every pivot delay is relative to an explicit anchor, and a
 * delay is trustworthy only at the leg that CAUSED it:
 *
 * - a `send` is origin-side, measured from the actor's previous step on the
 *   same leg (or `trigger` for the first) — unless an actor emit on ANOTHER leg
 *   is the nearer preceding instant and sits within the proximity window, in
 *   which case the send anchors THERE: two near-simultaneous originations have
 *   an order the capture states and independent anchors cannot hold, each chain
 *   drifting by its own relay latencies (§6.8);
 * - an `expect` that has a cross-leg emit of the same message type within the
 *   proximity window is a relay: `propagated`, ~0, anchored at that send, and
 *   NEVER timer-linked — the 0 is synthetic and the hop's latency belongs to
 *   whichever system replays the capture, so there is nothing to measure it
 *   against;
 * - an `expect` with no such origin was minted by the SUT: `sut-originated`,
 *   measured on this leg from the latest earlier step of its OWN transaction —
 *   a latency is measured inside the transaction that ran it, and an unrelated
 *   transaction interleaving on the leg is no milestone to time it from. Only
 *   where the transaction has no earlier step does the leg's previous step
 *   serve. `propagated` is never guessed.
 *
 * A step that OPENS its leg has no instant on that leg to measure from and
 * anchors on the document's own predecessor instead (`headAnchorOf`), never on
 * the run's start: the trigger is an instant the capture did not measure, and
 * claiming it discards the gap (§6.8).
 *
 * The CAUSALITY is this module's own reading and is not a pivot field: the
 * emitted delay keeps the anchor and drops the reason it was chosen.
 */

/** Why a delay is anchored where it is. */
export type DelayCausality = "measured" | "propagated" | "sut-originated"

/** One classified delay: the anchor, the dwell, and why. */
export interface ClassifiedDelay {
  readonly ms: number
  /** `trigger`, or `step:<n>` — the step's 1-based POSITION, not its id. */
  readonly from: string
  readonly timer_linked: boolean
  readonly derived: DelayCausality
}

/** A relay is instantaneous within this window (µs). */
export const PROXIMITY_US = 2_000_000

export interface StepTiming {
  readonly leg: string
  /** Whether the actor EMITS this message. */
  readonly emits: boolean
  readonly ts_us: number
  /** Cross-leg correlation key (`req:INVITE`, `resp:200:INVITE`) — never CSeq. */
  readonly typeKey: string
  /** The captured CSeq number: with `typeKey`, the transaction the message rides. */
  readonly cseq: number
  readonly timerLinked: boolean
}

/**
 * The index of the cross-leg emit that relayed into this arrival — the latest of
 * the same message type inside the proximity window — or -1 where the SUT minted
 * the message itself.
 *
 * ONE definition of "which emission this arrival is the relay of", shared by the
 * delay classification and by the pass that gives an unrelayed emission its
 * arrival: a relay that reads one way here and another there is how a document
 * comes to state two things about one event.
 */
export const relayOriginOf = (steps: ReadonlyArray<StepTiming>, i: number): number => {
  const s = steps[i]!
  let originIdx = -1
  for (let p = 0; p < i; p++) {
    const c = steps[p]!
    if (!c.emits || c.leg === s.leg || c.typeKey !== s.typeKey) continue
    if (s.ts_us - c.ts_us >= PROXIMITY_US) continue
    if (originIdx < 0 || c.ts_us > steps[originIdx]!.ts_us) originIdx = p
  }
  return originIdx
}

/** The transaction a step rides on its leg: the CSeq, method and number. */
const transaction = (s: StepTiming): string =>
  `${s.cseq}:${/^(?:req|resp:\d+):(.+)$/.exec(s.typeKey)?.[1] ?? s.typeKey}`

/**
 * The latest earlier step on this leg riding the SAME transaction, or -1 where
 * this step opens it. It is what an arrival the SUT minted is measured from —
 * its request where the transaction has one, the previous answer on a ladder.
 */
const transactionPredecessorOf = (steps: ReadonlyArray<StepTiming>, i: number): number => {
  const key = transaction(steps[i]!)
  for (let p = i - 1; p >= 0; p--) {
    const c = steps[p]!
    if (c.leg === steps[i]!.leg && transaction(c) === key) return p
  }
  return -1
}

export const classify = (steps: ReadonlyArray<StepTiming>): Array<ClassifiedDelay> => {
  const out: Array<ClassifiedDelay> = []
  steps.forEach((s, i) => {
    const anchored = (derived: DelayCausality): ClassifiedDelay => {
      let prev = derived === "sut-originated" ? transactionPredecessorOf(steps, i) : -1
      for (let p = i - 1; prev < 0 && p >= 0; p--) {
        if (steps[p]!.leg === s.leg) {
          prev = p
          break
        }
      }
      if (prev >= 0) {
        const ms = Math.floor((s.ts_us - steps[prev]!.ts_us) / 1000)
        return { ms, from: `step:${prev + 1}`, timer_linked: s.timerLinked, derived }
      }
      const head = headAnchorOf(steps, i)
      const ms = head < 0 ? 0 : Math.floor((s.ts_us - steps[head]!.ts_us) / 1000)
      return { ms, from: head < 0 ? "trigger" : `step:${head + 1}`, timer_linked: false, derived }
    }
    if (s.emits) {
      out.push(coincident(steps, i) ?? anchored("measured"))
      return
    }
    const originIdx = relayOriginOf(steps, i)
    out.push(
      originIdx >= 0
        ? { ms: 0, from: `step:${originIdx + 1}`, timer_linked: false, derived: "propagated" }
        : anchored("sut-originated")
    )
  })
  return out
}

/**
 * The anchor for a step that OPENS its leg: the step immediately before it in
 * the document — necessarily on another leg, since this one has none earlier —
 * or -1 for the case's first step, whose anchor is the trigger.
 *
 * A leg's head has no dwell on its own leg, and the run's start is not what the
 * capture measured it from: a serial reroute's second attempt opens at the
 * instant the first one failed, forty seconds in, and `trigger + 0` states both
 * that and case start at once. §6.8 opens an expect's budget at its anchor, so
 * the zero spends the whole budget across the very gap the document states. The
 * predecessor is the causal barrier the capture holds — for a second attempt,
 * the last step of the failed one's leg (§14's `attempts[].leg` + `position`) —
 * it points backwards as every anchor must, and the dwell across it is
 * SUT-relative where an offset from the trigger would pin captured absolute
 * time.
 *
 * A SEND an earlier arrival already awaits keeps the trigger: a send fires only
 * once its anchor completes, so anchoring it on a step waiting on it in turn
 * would never fire it at all (`awaited`, and the same cycle `coincident`
 * refuses).
 *
 * A head dwell is NEVER timer-linked, whatever the message offers: it is
 * measured across two legs and no system timer runs across two, so the value is
 * the SUT's own decision latency and §9.2 would hold a fresh SUT to the captured
 * platform's.
 */
const headAnchorOf = (steps: ReadonlyArray<StepTiming>, i: number): number =>
  i > 0 && !(steps[i]!.emits && awaited(steps, i)) ? i - 1 : -1

/**
 * Whether a cross-leg arrival listed BEFORE this send has no origin but it. An
 * arrival the capture shows ahead of every emit that could have relayed into it
 * reads `sut-originated`, and this send is then the only thing in the document
 * that might still supply it — so the send keeps a chain that arrival is not on.
 * Re-anchoring it across the legs would put the two in a cycle, each the other's
 * release. An arrival that already HAS an origin is nobody's hostage, however
 * many later polls of the same method the call goes on to make.
 */
const awaited = (steps: ReadonlyArray<StepTiming>, i: number): boolean => {
  const s = steps[i]!
  for (let p = 0; p < i; p++) {
    const e = steps[p]!
    if (e.emits || e.leg === s.leg || e.typeKey !== s.typeKey) continue
    if (relayOriginOf(steps, p) < 0) return true
  }
  return false
}

/**
 * The anchor for a send whose nearest preceding instant is an actor emit on
 * another leg, within `PROXIMITY_US`. Two originations that close together have
 * an order only the capture states: anchored on their own legs they hang off
 * whatever the SUT's relay latencies did upstream, and any difference from the
 * captured platform's inverts them. Anchoring the later on
 * the earlier makes the order the document's own. Only an EMIT qualifies as the
 * anchor — its instant is the harness's to choose, where an arrival's is the
 * SUT's, which is the quantity being escaped — and never for a send some
 * earlier arrival already awaits (`awaited`).
 */
const coincident = (
  steps: ReadonlyArray<StepTiming>,
  i: number
): ClassifiedDelay | undefined => {
  const s = steps[i]!
  let sameLegTs = -Infinity
  for (let p = i - 1; p >= 0; p--) {
    if (steps[p]!.leg === s.leg) {
      sameLegTs = steps[p]!.ts_us
      break
    }
  }
  for (let p = i - 1; p >= 0; p--) {
    const c = steps[p]!
    if (!c.emits || c.leg === s.leg) continue
    if (c.ts_us <= sameLegTs) return undefined
    if (s.ts_us - c.ts_us >= PROXIMITY_US) return undefined
    if (awaited(steps, i)) return undefined
    return {
      ms: Math.floor((s.ts_us - c.ts_us) / 1000),
      from: `step:${p + 1}`,
      timer_linked: s.timerLinked,
      derived: "measured"
    }
  }
  return undefined
}

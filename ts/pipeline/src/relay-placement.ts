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
 * (RFC 3261 §17.1.1.3) and relays nothing. Three moves, each keyed on content,
 * never on time alone:
 *
 * - **A relay stamped before its source, by its description** — an arrival no
 *   listed emission explains, carrying the session description of an emission
 *   of the same message type on another leg stamped at most `RELAY_SKEW_US`
 *   later — is moved to just after that emission, past no step of its own leg
 *   but another provisional stamped before whatever it relays. The origin line
 *   names one session version, where a bare message names nothing a minted
 *   one could not equally carry.
 * - **A relay stamped before its source, by its identifying values** — on a
 *   platform that relays provisionals as they arrive, an arrival no listed
 *   emission explains, carrying identifying header values (`RelayMarker`) no
 *   earlier step carried, sent or received, all of which an emission of the
 *   same message type on another leg, and only that one, delivers at most
 *   `RELAY_SKEW_US` later, before that leg's final to its INVITE — is moved to
 *   just after that emission. A value no step carried before is the far
 *   side's: the platform did not state it first. These moves follow every
 *   move by description. One
 *   passes at most one step of the arrival's leg, a relayed provisional of
 *   its transaction whose source is listed, and no step of a third leg or of
 *   another transaction on the source's leg; and it leaves every relay of the
 *   arrival's transaction whose source is known in the order of the sources,
 *   or it is not made.
 * - **Relays out of their sources' order** — two arrivals of one message type,
 *   adjacent on one leg, whose sources run the other way — swap places.
 *
 * Runs right after the steps are built, before any pass that reads positions.
 * Mutates `steps`, `timings` and `sources` together and renumbers the ids.
 */
import { relayedProvisional, relayOriginOf, transactionOf, type StepTiming } from "./delay.js"
import type { StepDraft } from "./draft.js"
import { stepId, type StepSource } from "./flowsteps.js"
import { identifiesSession } from "./relay-image.js"

/**
 * Whether a step of `leg` between positions `a` and `b` stops a move by
 * session description: any but another relayed provisional no listed emission
 * explains whose own source — a same-type emission with its content, bare
 * included, on another leg — stands between it and the mover's source. A
 * provisional the system under test made, or one relaying a later source,
 * keeps the mover in front of it.
 */
const blocksSessionMove = (timings: ReadonlyArray<StepTiming>, leg: string, a: number, b: number): boolean =>
  timings.slice(a + 1, b).some((t, k) => {
    if (t.leg !== leg) return false
    const at = a + 1 + k
    const earlyRelay =
      relayedProvisional(t) &&
      !t.emits &&
      relayOriginOf(timings, at) < 0 &&
      timings.slice(at + 1, b).some((c) => c.emits && c.leg !== leg && c.typeKey === t.typeKey && c.image === t.image)
    return !earlyRelay
  })

/**
 * How much earlier than its source a vantage capturing the two directions on
 * different interfaces may stamp a relay (µs). A voluntary bound, not a
 * measured one: a pair stamped further apart is left where the capture put it.
 */
export const RELAY_SKEW_US = 500_000

/**
 * The emission the arrival at `i` relays by its session description, or -1:
 * the first emission of its type and image on another leg, stamped at most
 * `RELAY_SKEW_US` after it.
 */
const sessionSourceOf = (timings: ReadonlyArray<StepTiming>, i: number): number => {
  const s = timings[i]!
  for (let k = i + 1; k < timings.length; k++) {
    const c = timings[k]!
    if (c.ts_us - s.ts_us > RELAY_SKEW_US) return -1
    if (c.emits && c.leg !== s.leg && c.typeKey === s.typeKey && c.image === s.image) return k
  }
  return -1
}

const finalToInvite = (t: StepTiming): boolean => /^resp:[2-6]\d\d:INVITE$/.test(t.typeKey)

/**
 * The emission the arrival at `i` relays by its identifying values, or -1: the
 * one emission of the arrival's type on another leg, stamped at most
 * `RELAY_SKEW_US` after it and before its leg's final to its INVITE, that
 * delivers every value of the arrival no step listed before it carried, an
 * emission or an arrival: a value the platform stated first is its own. An
 * arrival with no such value has none, and one whose values two emissions
 * deliver has none either: the values do not single its source out.
 */
const valuesSourceOf = (timings: ReadonlyArray<StepTiming>, i: number): number => {
  const s = timings[i]!
  const delivered = new Set(timings.slice(0, i).flatMap((t) => t.marks ?? []))
  const unseen = (s.marks ?? []).filter((m) => !delivered.has(m))
  if (unseen.length === 0) return -1
  const ended = new Set<string>()
  let source = -1
  for (let k = i + 1; k < timings.length; k++) {
    const c = timings[k]!
    if (c.ts_us - s.ts_us > RELAY_SKEW_US) break
    if (c.leg === s.leg) continue
    const transaction = `${c.leg}\u0000${transactionOf(c)}`
    if (finalToInvite(c)) ended.add(transaction)
    const delivers = unseen.every((m) => c.marks?.includes(m) === true)
    if (!c.emits || c.typeKey !== s.typeKey || ended.has(transaction) || !delivers) continue
    if (source >= 0) return -1
    source = k
  }
  return source
}

/**
 * Whether the step at `i` is a relayed provisional no listed emission
 * explains: the one kind of step this pass moves behind a later source.
 */
const earlyRelay = (timings: ReadonlyArray<StepTiming>, i: number): boolean => {
  const s = timings[i]!
  return !s.emits && relayedProvisional(s) && relayOriginOf(timings, i) < 0
}

/**
 * The emission the arrival at `i` relays, or -1 where none is known: its
 * listed origin, else the later source its session description or its
 * identifying values name.
 */
const knownSourceOf = (timings: ReadonlyArray<StepTiming>, i: number): number => {
  const origin = relayOriginOf(timings, i)
  if (origin >= 0) return origin
  const bySession = identifiesSession(timings[i]!.image) ? sessionSourceOf(timings, i) : -1
  return bySession >= 0 ? bySession : valuesSourceOf(timings, i)
}

/**
 * Whether listing the arrival at `a` just after its source at `b` keeps every
 * relayed provisional of its leg and transaction in the order of the sources
 * they relay: each one listed before it then relays an earlier emission, each
 * one listed after it a later one. A relay whose source is unknown constrains
 * nothing.
 */
const keepsSourceOrder = (timings: ReadonlyArray<StepTiming>, a: number, b: number): boolean => {
  const s = timings[a]!
  return timings.every((t, k) => {
    if (k === a || t.leg !== s.leg || t.emits || !relayedProvisional(t) || transactionOf(t) !== transactionOf(s)) return true
    const source = knownSourceOf(timings, k)
    if (source < 0) return true
    return k > b ? source > b : source < b
  })
}

/**
 * Whether a step between positions `a` and `b` stops a move by identifying
 * values: a second step of the arrival's leg, one that is not a relayed
 * provisional of its transaction with a listed source, a step of a third leg,
 * or one of another transaction on the source's leg.
 */
const blocksValuesMove = (timings: ReadonlyArray<StepTiming>, a: number, b: number): boolean => {
  const [s, source] = [timings[a]!, timings[b]!]
  let own = 0
  return timings.slice(a + 1, b).some((t, k) => {
    if (t.leg === source.leg) return transactionOf(t) !== transactionOf(source)
    if (t.leg !== s.leg) return true
    own++
    const listedRelay =
      relayedProvisional(t) && !t.emits && transactionOf(t) === transactionOf(s) && relayOriginOf(timings, a + 1 + k) >= 0
    return own > 1 || !listedRelay
  })
}

/**
 * Every move by session description, last arrival to first: a later early
 * relay is placed before an earlier one tries to pass it, so two early answers
 * both land behind their own sources. `move(from, to)` lists the step at
 * `from` just after the one at `to`.
 */
const placeBySession = (timings: ReadonlyArray<StepTiming>, move: (from: number, to: number) => void): void => {
  for (let i = timings.length - 1; i >= 0; i--) {
    if (!earlyRelay(timings, i) || !identifiesSession(timings[i]!.image)) continue
    const source = sessionSourceOf(timings, i)
    if (source >= 0 && !blocksSessionMove(timings, timings[i]!.leg, i, source)) move(i, source)
  }
}

/**
 * Every move by identifying values, last arrival to first, once every move by
 * session description is made: a relay placed by its values never stands in
 * the way of one its description places. An arrival whose description names a
 * source stays where that move left it, and a move that would list the
 * arrival's leg out of its sources' order is not made.
 */
const placeByValues = (timings: ReadonlyArray<StepTiming>, move: (from: number, to: number) => void): void => {
  for (let i = timings.length - 1; i >= 0; i--) {
    if (!earlyRelay(timings, i)) continue
    if (identifiesSession(timings[i]!.image) && sessionSourceOf(timings, i) >= 0) continue
    const source = valuesSourceOf(timings, i)
    if (source < 0 || blocksValuesMove(timings, i, source) || !keepsSourceOrder(timings, i, source)) continue
    move(i, source)
  }
}

/** One arrival this pass moved, for the flag. */
export interface Placed {
  readonly step: StepDraft
  readonly source: StepDraft
  readonly why: "stamped-before-source" | "stamped-before-values-source" | "out-of-source-order"
}

export interface PlacementInput {
  readonly steps: Array<StepDraft>
  readonly timings: Array<StepTiming>
  readonly sources: Array<StepSource>
  /**
   * Whether the captured platform relayed its provisionals as they arrived:
   * only then does a provisional carrying the far side's values relay the
   * emission that delivers them.
   */
  readonly transparent18x: boolean
}

export const placeRelaysAfterSources = (input: PlacementInput): Array<Placed> => {
  const { steps, timings, sources, transparent18x } = input
  const placed: Array<Placed> = []
  /**
   * Lists the step at `from` just after the one at `to` (> `from`): removing
   * it shifts `to` down by one. Every step below `from` keeps its place.
   */
  const move = (from: number, to: number): void => {
    for (const arr of [steps, timings, sources] as Array<Array<unknown>>) {
      const [item] = arr.splice(from, 1)
      arr.splice(to, 0, item)
    }
  }

  placeBySession(timings, (from, to) => {
    placed.push({ step: steps[from]!, source: steps[to]!, why: "stamped-before-source" })
    move(from, to)
  })
  if (transparent18x) {
    placeByValues(timings, (from, to) => {
      placed.push({ step: steps[from]!, source: steps[to]!, why: "stamped-before-values-source" })
      move(from, to)
    })
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

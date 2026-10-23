/**
 * Reading a flows document a window of legs at a time.
 *
 * A capture of thousands of calls has a document no response carries whole and
 * no ladder draws whole, so a reader asks for the legs it is about and holds
 * the rest as ABSENT: the legs it did read keep the indices the document gives
 * them, which are the indices a case, a URL and a finding cite.
 */

/** Legs of a document one ladder draws at once. */
export const LADDER_LEGS = 200

/** Legs one response carries where the request states no window of its own. */
export const SERVED_LEGS = 64

/** Legs one response carries at most: no request may ask a document back whole. */
export const SERVED_LEGS_MAX = 256

export interface FlowsWindow {
  /** Index of the window's first leg in the document. */
  readonly from: number
  readonly count: number
}

const clamp = (value: number, limit: number): number =>
  Math.min(Math.max(0, Math.trunc(value)), limit)

/**
 * The window a ladder draws: it opens on the first leg the ladder is about —
 * the legs a case cites — and on the document's own first leg where it is
 * about none.
 */
export const ladderWindow = (
  total: number,
  focus: readonly number[],
  budget: number = LADDER_LEGS
): FlowsWindow => {
  const inside = focus.filter((leg) => leg >= 0 && leg < total)
  const from = inside.length === 0 ? 0 : clamp(Math.min(...inside), Math.max(0, total - 1))
  return { from, count: Math.max(0, Math.min(budget, total - from)) }
}

/**
 * The window a REQUEST is served: capped, so no request asks the document back
 * whole, and held inside the document, so a window past the last leg is an
 * empty window at its end rather than a refusal.
 */
export const servedWindow = (total: number, from: number, count?: number): FlowsWindow => {
  const start = clamp(from, Math.max(0, total))
  return {
    from: start,
    count: Math.min(clamp(count ?? SERVED_LEGS, SERVED_LEGS_MAX), Math.max(0, total) - start)
  }
}

/**
 * The windows that read exactly `legs` — a case's own — held inside the
 * document: one per run of consecutive legs, each at most `SERVED_LEGS_MAX`,
 * in leg order. What a ladder of the selected legs alone asks for: a case
 * whose legs lie further apart than one window holds is read whole all the
 * same, and a leg outside the document is asked for nowhere.
 */
export const legWindows = (total: number, legs: readonly number[]): readonly FlowsWindow[] => {
  const inside = [...new Set(legs.filter((leg) => Number.isInteger(leg) && leg >= 0 && leg < total))]
    .sort((left, right) => left - right)
  const windows: FlowsWindow[] = []
  for (const leg of inside) {
    const last = windows[windows.length - 1]
    if (last !== undefined && leg === last.from + last.count && last.count < SERVED_LEGS_MAX) {
      windows[windows.length - 1] = { from: last.from, count: last.count + 1 }
    } else {
      windows.push({ from: leg, count: 1 })
    }
  }
  return windows
}

/** The window `count` legs on from this one, kept inside the document. */
export const shiftWindow = (window: FlowsWindow, total: number, steps: number): FlowsWindow =>
  ladderWindow(total, [clamp(window.from + steps * Math.max(1, window.count), Math.max(0, total - 1))], window.count)

/** One window's legs as a read brought them back: from its first index, in document order. */
export interface FlowsChunk {
  readonly from: number
  readonly legs: readonly unknown[]
}

/**
 * A document read in windows, as a value a reader parses: the envelope's own
 * fields, and its legs at the indices the document gives them and nowhere else.
 */
export const chunkedFlows = (envelope: unknown, total: number, chunks: readonly FlowsChunk[]): unknown => {
  const sparse = new Array<unknown>(
    Math.max(total, ...chunks.map((chunk) => chunk.from + chunk.legs.length))
  )
  for (const chunk of chunks) {
    chunk.legs.forEach((leg, index) => {
      sparse[chunk.from + index] = leg
    })
  }
  return { ...(typeof envelope === "object" && envelope !== null ? envelope : {}), legs: sparse }
}

/** A document read one window at a time: `chunkedFlows` over that window. */
export const windowedFlows = (
  envelope: unknown,
  total: number,
  from: number,
  legs: readonly unknown[]
): unknown => chunkedFlows(envelope, total, [{ from, legs }])

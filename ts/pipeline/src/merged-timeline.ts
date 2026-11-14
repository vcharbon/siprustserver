/**
 * Several positions' timelines read as one (ADR-0061): every message the
 * positions hold, in capture order. A reader that orders a call's messages
 * across its legs reads them here; each timeline stays its position's own.
 */
import type { Entry, Timeline } from "./leg-facts.js"

/** One message of several positions: an entry, or one of the retransmissions folded into it. */
export interface Placed {
  /** The index, among the timelines merged, of the position that holds it. */
  readonly position: number
  /** The index of its leg in the flows document. */
  readonly leg: number
  /** The entry it is, or the entry it repeats. */
  readonly entry: Entry
  /** Which of the entry's `repeats` it is; `undefined` for the entry itself. */
  readonly repeat: number | undefined
  /** The source pointer: its index in its leg's `msgs`. */
  readonly msg: number
  readonly at_us: number
}

const byCaptureOrder = (a: Placed, b: Placed): number => a.at_us - b.at_us || a.leg - b.leg || a.msg - b.msg

/**
 * Every message the positions hold, each entry and each retransmission folded
 * into it, ordered by capture time, then leg index, then index in the leg. A
 * retransmission stands at its own capture time, so a copy captured before
 * the message it repeats stands before it. Reads no message of the document.
 */
export const merged = (timelines: ReadonlyArray<Timeline>): ReadonlyArray<Placed> => {
  const placed: Array<Placed> = []
  timelines.forEach((timeline, position) => {
    const leg = timeline.legIndex
    for (const entry of timeline.entries) {
      placed.push({ position, leg, entry, repeat: undefined, msg: entry.msg, at_us: entry.at_us })
      entry.repeats.forEach((r, repeat) => {
        placed.push({ position, leg, entry, repeat, msg: r.msg, at_us: r.at_us })
      })
    }
  })
  return placed.sort(byCaptureOrder)
}

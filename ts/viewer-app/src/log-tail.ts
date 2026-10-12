/**
 * The live log's tail state (§6 J4, §8 E4), pure: what the page holds
 * between polls and how one answer folds into it. The page asks for the
 * bytes from `from`, appends what came, and asks again from where the answer
 * said the next poll starts; an answer that starts before what is held is a
 * log that was replaced under the page, and the text starts over. A `done`
 * answer ends the polling. The text is capped so a page open on a long job
 * never holds gigabytes: the oldest lines go, and the cap says so.
 */
import type { JobLogRange } from "./api.ts"

export interface Tail {
  readonly text: string
  /** The offset the next poll asks from. */
  readonly from: number
  /** Nothing more will come: the job ended and the end was read. */
  readonly done: boolean
  /** Bytes dropped off the front to stay under the cap. */
  readonly dropped: number
}

export const EMPTY: Tail = { text: "", from: 0, done: false, dropped: 0 }

/** The most characters a page keeps; a log longer than this shows its last lines. */
export const CAP = 2_000_000

/** The text under the cap, cut at a line so the first line shown is whole. */
const capped = (text: string, cap: number): { readonly text: string; readonly dropped: number } => {
  if (text.length <= cap) return { text, dropped: 0 }
  const excess = text.length - cap
  // The first line end that takes at least `excess` characters with it.
  const newline = text.indexOf("\n", Math.max(0, excess - 1))
  const at = newline < 0 ? excess : newline + 1
  return { text: text.slice(at), dropped: at }
}

/** Folds one answer over the tail held. */
export const fold = (tail: Tail, range: JobLogRange, cap = CAP): Tail => {
  // A range that starts before what is held is a log that shrank or was replaced: start over.
  const restarted = range.from < tail.from
  const kept = capped((restarted ? "" : tail.text) + range.text, cap)
  return { text: kept.text, from: range.to, done: range.done, dropped: (restarted ? 0 : tail.dropped) + kept.dropped }
}

/** A chunk this big is a log being caught up, not a tail: the next poll follows at once. */
export const BIG_CHUNK = 256 * 1024

/** The poll interval after an answer: at once while catching up, else the tail's cadence. */
export const nextDelay = (range: JobLogRange, cadenceMs: number): number =>
  range.to - range.from >= BIG_CHUNK ? 0 : cadenceMs

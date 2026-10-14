/**
 * Which capture names are SETTLED: a name a ticket cites a case or a run of,
 * or one a real-clock lane replayed. A settled name's bytes never change under
 * it — a byte change mints the next generation — while any other name is
 * extracted again in place: a paused-clock run is remade in seconds and pins
 * nothing.
 */
import { Citations, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import { LaneHosting } from "../deployment/index.js"
import * as CitedReport from "../ids/cited-report.js"

/** The captures one lane ran. */
export interface Ran {
  readonly lane: string
  readonly captures: ReadonlyArray<string>
}

/**
 * The names runs and citations pin: the captures a pinning lane (one of
 * `pinning`, the real-clock lanes) ran, and the capture of every cited id.
 */
export const names = (
  ran: ReadonlyArray<Ran>,
  cited: ReadonlyArray<Pick<CitedReport.Line, "capture">>,
  pinning: ReadonlySet<string>
): ReadonlySet<string> => {
  const held = new Set(ran.filter((r) => pinning.has(r.lane)).flatMap((r) => r.captures))
  for (const line of cited) {
    if (line.capture !== null) held.add(line.capture)
  }
  return held
}

/** Every settled name of the store. */
export const captures = Effect.fn("Settled.captures")(function*() {
  const runs = yield* Runs.Service
  const citations = yield* Citations.Service
  const hosting = yield* LaneHosting.Service
  const pinning = new Set(LaneHosting.realClock(hosting))
  const ran: Array<Ran> = []
  for (const { lane } of yield* runs.lanes) {
    if (pinning.has(lane)) ran.push({ lane, captures: yield* runs.ranOn(lane) })
  }
  const cited = yield* CitedReport.report(yield* citations.all)
  return names(ran, cited, pinning)
})

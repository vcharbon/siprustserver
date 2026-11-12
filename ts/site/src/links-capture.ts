/**
 * The overview of a capture's study URL (no `case`): a router to its calls,
 * never a reading of the capture. A capture of thousands of calls is read one
 * call at a time, so this states, per lane, the standing run's failed calls,
 * each linked to its own overview, and how to name any other call — and links
 * no capture-wide resource. It reads the run store's standing rows alone, so
 * it answers fast whatever the capture's size.
 */
import { Runs } from "@sip/store"
import { absolute, href, linksHref, type Overview, type OverviewSection } from "@sip/viewer-core/routes"
import { Effect } from "effect"
import { lanes } from "./lanes.ts"
import { SitePack } from "./site-pack.ts"

/** The most failed calls a lane lists; the rest are counted. */
export const MAX_CALLS = 50

/** The page overview of the capture `capture`'s study, as a router to its calls. */
export const captureOverview = Effect.fn("Links.captureOverview")(function*(
  base: Overview,
  capture: string,
  origin: string
) {
  const runs = yield* Runs.Service
  const read = yield* Effect.forEach(lanes((yield* SitePack.Service).lanes), (one) =>
    runs.standing(capture, one.lane).pipe(
      Effect.map((standing) => ({ lane: one.lane, standing })),
      Effect.orElseSucceed(() => ({ lane: one.lane, standing: null }))
    ))
  const sections = read.flatMap(({ lane, standing }): ReadonlyArray<OverviewSection> => {
    if (standing === null) return []
    const failed = standing.caseResults.filter((one) => !one.passed && one.skipped === null)
    return [{
      heading: `${lane} · run ${standing.runId}: ${failed.length} of ${standing.cases} calls failed`,
      lines: [
        `run ${standing.runId} · ${standing.status} · finished ${standing.finishedAt ?? "—"}` +
        ` · ${standing.skipped} skipped`,
        ...(failed.length > MAX_CALLS ? [`the first ${MAX_CALLS} failed calls linked; ${failed.length - MAX_CALLS} more`] : [])
      ],
      links: failed.slice(0, MAX_CALLS).map((one) => ({
        label: `${one.caseId}${one.failedStep === null ? "" : ` (failed at ${one.failedStep})`}`,
        href: linksHref(origin, absolute(origin, href.triageStudy(capture, { case: one.caseId, lane })))
      }))
    }]
  })
  return {
    ...base,
    title: `Calls of capture ${capture}`,
    status: [
      "A capture is read one call at a time: each call below links its own overview.",
      `Any other call: its page ${origin}/triage/${encodeURIComponent(capture)}?case=${capture}-k<hash>&lane=<lane>, ` +
      `its overview ${origin}/api/links?url=<that page URL, URL-encoded>`,
      ...(sections.length === 0 ? ["no lane has a standing run of this capture"] : [])
    ],
    sections
  } satisfies Overview
})

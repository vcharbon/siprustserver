/**
 * The link to one case's call details on one run and lane: the campaign, its
 * environment, the run, the case and the delta, none of the page's filters,
 * tab or selection, so whoever opens it lands on that modal alone.
 */
import { NO_SELECTION } from "./campaign-facets.ts"
import { paramsOf } from "./campaign-state.ts"
import { href } from "./router.ts"

/** The call details `at` names, as an absolute URL under `origin`. */
export const caseLink = (origin: string, capture: string, at: {
  readonly env: string | undefined
  readonly caseId: string
  readonly run: number
  readonly lane: string
}): string =>
  `${origin}${
    href.campaign(
      capture,
      paramsOf({
        env: at.env,
        tab: "failures",
        run: at.run,
        vs: undefined,
        filters: { lanes: [], failing: false, oneLane: false, fresh: false, text: "" },
        selection: NO_SELECTION,
        caseId: at.caseId,
        delta: { run: at.run, lane: at.lane }
      })
    )
  }`

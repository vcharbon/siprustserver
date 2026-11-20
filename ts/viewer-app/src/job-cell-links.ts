/**
 * Where a job's red cell is read. A capture replay's cell is a call of a
 * capture: its capture and its call open the triage study on the cell's lane.
 * A campaign's cell is a case of a campaign run, which no triage study holds:
 * its campaign opens that run's page, its case the case's call details on the
 * cell's run and lane.
 */
import type { JobRedCell } from "./api.ts"
import { NO_SELECTION } from "./campaign-facets.ts"
import { caseHref } from "./campaign-case-link.ts"
import { paramsOf } from "./campaign-state.ts"
import { href } from "./router.ts"

/** The page of what the cell belongs to: the capture's study, or the campaign run's page. */
export const holderHref = (cell: JobRedCell): string =>
  cell.campaign === null
    ? href.triageStudy(cell.capture, { lane: cell.lane })
    : href.campaign(
      cell.capture,
      paramsOf({
        env: cell.campaign.env ?? undefined,
        tab: "failures",
        run: cell.campaign.run,
        vs: undefined,
        filters: { lanes: [], failing: false, oneLane: false, fresh: false, text: "" },
        selection: NO_SELECTION,
        caseId: undefined,
        delta: undefined
      })
    )

/** The cell itself: the call's study, or the case's call details on its run and lane. */
export const cellHref = (cell: JobRedCell): string =>
  cell.campaign === null
    ? href.triageStudy(cell.capture, { lane: cell.lane, case: cell.caseId })
    : caseHref(cell.capture, { env: cell.campaign.env ?? undefined, caseId: cell.caseId, run: cell.campaign.run, lane: cell.lane })

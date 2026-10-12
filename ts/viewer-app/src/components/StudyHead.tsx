/**
 * The study page's title and its first line. Of a capture: its name and the
 * counts of its calls. Of ONE call: the case id, over a pointer up to the
 * capture study — the page a call opened from the queue is not the capture's,
 * and the way back to it must be the first thing read.
 */
import { parseExclusions } from "@sip/viewer-core"
import type { JSX } from "react"
import type { TriageStudy } from "../api.ts"
import { runCounts } from "../run-counts.ts"
import { href } from "../router.ts"
import { Link } from "./Link.tsx"

/**
 * The capture in numbers, one line: its calls, and how the standing run ruled
 * on each. With a thousand calls the chips below are no count.
 */
const CountsLine = ({ study }: { study: TriageStudy }): JSX.Element => {
  const read = study.exclusions === undefined ? undefined : parseExclusions(study.exclusions.document)
  const counts = runCounts(study.standing, read?.ok ? read.records : undefined, study.cut?.cases)
  return (
    <p className="counts run-counts">
      <span className="tag">{counts.calls} calls</span>
      <span className="tag muted">{counts.refused} refused</span>
      {study.standing !== undefined && (
        <>
          <span className="tag status-passing">{counts.green} green</span>
          <span className="tag missing">{counts.red} red</span>
          <span className="tag muted">{counts.skipped} skipped</span>
        </>
      )}
    </p>
  )
}

export const StudyHead = (
  { study, lane, cell }: {
    study: TriageStudy
    /** The lane the page reads on: the capture study opens on the same one. */
    lane: string | undefined
    /** The replay on screen, `<run>/<cell>`: the capture study opens on it too. */
    cell?: string | undefined
  }
): JSX.Element => {
  const disposition = <span className={`tag disposition-${study.disposition}`}>{study.disposition}</span>
  if (study.case === undefined) {
    return (
      <>
        <h1>{disposition}{study.capture}</h1>
        <CountsLine study={study} />
      </>
    )
  }
  return (
    <>
      <h1>{disposition}{study.case}</h1>
      <p className="counts run-counts">
        <span className="muted">one call of</span>
        <Link to={href.triageStudy(study.capture, { lane, cell })}>{study.capture}</Link>
        <span className="muted">— the capture study lists every call</span>
      </p>
    </>
  )
}

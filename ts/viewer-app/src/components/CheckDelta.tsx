/**
 * One case's delta on one run and lane: the failed checks as large cards, each
 * with its decision line and the cell's decision-service log beside it, then
 * the ladder of what the run produced and the HTTP exchanges, the selected
 * message above the ladder.
 */
import { buildRecordingLayout, type LadderEvent, NO_HTTP_EXCHANGES, parseRecording } from "@sip/viewer-core"
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { api, type Campaign, type CampaignResult, useAsync } from "../api.ts"
import { cellDocument } from "../opening.ts"
import { CheckCard } from "./CheckCard.tsx"
import { Exchange } from "./Exchange.tsx"
import { LadderPane } from "./LadderPane.tsx"
import { SipMessageView } from "./SipMessageView.tsx"

const logPath = (capture: string, session: string, cell: string) =>
  `/api/campaign/${encodeURIComponent(capture)}/runs/${encodeURIComponent(session)}/cells/${encodeURIComponent(cell)}/log`

const LogLine = ({ capture, session, result }: {
  readonly capture: string
  readonly session: string
  readonly result: CampaignResult
}): JSX.Element | null => {
  if (result.cell === null || result.log === null) return null
  if (result.log.missing !== undefined) return <span className="cp-log muted"> · log missing: {result.log.missing}</span>
  return (
    <a className="cp-log" href={logPath(capture, session, result.cell)} target="_blank" rel="noreferrer">
      {" "}· log ({result.log.records})
    </a>
  )
}

const Ladder = ({ capture, session, cell }: { readonly capture: string; readonly session: string; readonly cell: string }) => {
  const detail = useAsync(() => api.triageCell(capture, session, cell), `${capture}/${session}/${cell}`)
  const [selected, setSelected] = useState<string | undefined>(undefined)
  const value = detail.state === "ok" ? detail.value : undefined
  const layout = useMemo(() => {
    const recording = parseRecording(value?.recording ?? {})
    const document = cellDocument(value?.pivot, undefined)
    return buildRecordingLayout(recording, {
      document: document.ok ? document.document : undefined,
      title: cell,
      subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages`
    })
  }, [value, cell])
  if (detail.state === "loading") return <p className="muted">…</p>
  if (detail.state === "error") return <p className="cp-nok">{detail.message}</p>
  const picked = layout.events.find((event: LadderEvent) => event.id === selected)
  return (
    <>
      <LadderPane
        layout={layout}
        heading="Ladder"
        selectedId={selected}
        onSelect={(event) => setSelected(event.id)}
        legend={false}
        detail={picked === undefined ? undefined : <SipMessageView record={picked.source} label={picked.id} />}
      />
      <h3>HTTP</h3>
      <Exchange
        exchanges={(value?.http ?? NO_HTTP_EXCHANGES).exchanges}
        warnings={(value?.http ?? NO_HTTP_EXCHANGES).warnings}
      />
    </>
  )
}

export const CheckDelta = ({ view, caseId, run, lane, onClose }: {
  readonly view: Campaign
  readonly caseId: string
  readonly run: number
  readonly lane: string
  readonly onClose: () => void
}): JSX.Element => {
  const laneRun = view.runs.find((r) => r.id === run)?.lanes.find((l) => l.lane === lane)
  const result = laneRun?.results.find((r) => r.caseId === caseId)
  return (
    <div className="cp-modal" onClick={(event) => event.target === event.currentTarget && onClose()}>
      <div className="cp-modal-body">
        <button type="button" className="cp-close" onClick={onClose} title="close (Esc)">✕</button>
        <h2>Check delta</h2>
        <p className="muted mono">#{run} · {lane} · {caseId}</p>
        {laneRun === undefined || result === undefined ? <p className="muted">— not run</p> : (
          <>
            {result.outcome === "pass" && <p className="cp-ok">✓ every check</p>}
            {result.outcome === "error" && <pre className="cp-pre">{result.detail}</pre>}
            {result.checks.map((check, at) => (
              <CheckCard
                key={at}
                check={check}
                sides={view.sides}
                saidLabel={view.saidLabel}
                said={<LogLine capture={view.capture} session={laneRun.session} result={result} />}
              />
            ))}
            {result.checks.length === 0 && result.outcome !== "error" && (
              <p className="mono">
                {view.saidLabel}
                <LogLine capture={view.capture} session={laneRun.session} result={result} />
              </p>
            )}
            {result.cell !== null && <Ladder capture={view.capture} session={laneRun.session} cell={result.cell} />}
          </>
        )}
      </div>
    </div>
  )
}

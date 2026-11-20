/**
 * One case's delta on one run and lane: the cell's verification lines and
 * status, the failed checks as large cards, each
 * with its decision line and the attachment it was read from beside it, the
 * cell's attachments, the on-demand collectors its lane offers, and its records compared with the case's other
 * sub-cases or lanes, then the ladder of what the run produced and the HTTP exchanges, the
 * selected message above the ladder. A check's anchor (the datagram it names
 * where one arrived, else its step's message) and an exchange's preceding
 * message select a ladder message; the header copies the modal's own link.
 */
import { buildRecordingLayout, type LadderEvent, NO_HTTP_EXCHANGES, parseRecording } from "@sip/viewer-core"
import type { JSX } from "react"
import { useMemo, useRef, useState } from "react"
import {
  api,
  type AttachmentListed,
  attachmentFileHref,
  type Campaign,
  type CampaignLane,
  type CampaignResult,
  useAsync
} from "../api.ts"
import { arrivedMessages } from "../campaign-arrived-message.ts"
import { caseLink } from "../campaign-case-link.ts"
import { checkMessages } from "../campaign-check-message.ts"
import { statusOf } from "../campaign-facets.ts"
import { exchangesAfter } from "../exchange-after.ts"
import { cellDocument } from "../opening.ts"
import { useLinkOrigin } from "../server-config.ts"
import { useCopy } from "../use-copy.ts"
import { AttachmentList } from "./attachments/AttachmentList.tsx"
import { collectCellOf, CollectOnDemand } from "./attachments/CollectOnDemand.tsx"
import { CheckCard } from "./CheckCard.tsx"
import { Exchange } from "./Exchange.tsx"
import { LadderPane } from "./LadderPane.tsx"
import { RecordsCompare } from "./RecordsCompare.tsx"
import { scrollTopFor } from "./scroll.ts"
import { SipMessageView } from "./SipMessageView.tsx"
import { VerificationLines } from "./VerificationLines.tsx"

/** The count of what `said` was read from, linked to the cell's attachment `log` where it holds it. */
const LogLine = ({ result, log }: {
  readonly result: CampaignResult
  readonly log: AttachmentListed | undefined
}): JSX.Element | null => {
  if (result.cell === null || result.log === null) return null
  if (result.log.missing !== undefined) return <span className="cp-log muted"> · log missing: {result.log.missing}</span>
  if (log === undefined) return <span className="cp-log muted"> · log ({result.log.records})</span>
  return (
    <a className="cp-log" href={attachmentFileHref(log.id)} target="_blank" rel="noreferrer">
      {" "}· log ({result.log.records})
    </a>
  )
}

/** The case's checks, attachments and ladder; a check made at a message the ladder holds links to it. */
const CallDetails = ({ view, laneRun, result, caseId, run, lane }: {
  readonly view: Campaign
  readonly laneRun: CampaignLane
  readonly result: CampaignResult
  readonly caseId: string
  readonly run: number
  readonly lane: string
}): JSX.Element => {
  const { capture } = view
  const { session } = laneRun
  const cell = result.cell
  const detail = useAsync(
    () => (cell === null ? Promise.resolve(undefined) : api.triageCell(capture, session, cell)),
    `${capture}/${session}/${cell ?? ""}`
  )
  const [landed, setLanded] = useState(0)
  const attached = useAsync(
    () => (cell === null ? Promise.resolve([]) : api.attachments({ run: laneRun.runId, case: caseId })),
    `${laneRun.runId}/${caseId}/${cell ?? ""}/${landed}`
  )
  const listed = attached.state === "ok" ? attached.value : []
  const said = listed.find((one) => one.name === view.saidLog)
  const [selected, setSelected] = useState<string | undefined>(undefined)
  const ladderRef = useRef<HTMLDivElement | null>(null)
  const value = detail.state === "ok" ? detail.value : undefined
  const recording = useMemo(() => (value === undefined ? undefined : parseRecording(value.recording ?? {})), [value])
  const layout = useMemo(() => {
    if (value === undefined || recording === undefined) return undefined
    const document = cellDocument(value.pivot, undefined)
    return buildRecordingLayout(recording, {
      document: document.ok ? document.document : undefined,
      title: cell ?? "",
      subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages`
    })
  }, [value, recording, cell])
  const http = value?.http ?? NO_HTTP_EXCHANGES
  const targets = useMemo(() => {
    const arrived = arrivedMessages(result.checks, value?.verdict, recording?.messages ?? [])
    const stepped = checkMessages(result.checks, layout?.events ?? [])
    return result.checks.map((_, at) => arrived[at] ?? stepped[at])
  }, [result.checks, value, recording, layout])
  const after = useMemo(() => exchangesAfter(http.exchanges, layout?.events ?? []), [http, layout])
  const show = (id: string): void => {
    setSelected(id)
    ladderRef.current?.scrollIntoView({ block: "start" })
    // The pane scrolls its box on a change of selection; the same one again is brought back here.
    const box = ladderRef.current?.querySelector<HTMLElement>(".ladder-scroll") ?? undefined
    const y = layout?.events.find((event) => event.id === id)?.y
    if (id !== selected || box === undefined || y === undefined) return
    const top = scrollTopFor(box, y, undefined)
    if (top !== undefined) box.scrollTo({ top })
  }
  const picked = layout?.events.find((event: LadderEvent) => event.id === selected)
  return (
    <>
      {result.lines !== undefined && (
        <VerificationLines lines={result.lines} status={statusOf(result)} sides={view.sides} />
      )}
      {result.outcome === "pass" && <p className="cp-ok">✓ every check</p>}
      {result.outcome === "error" && <pre className="cp-pre">{result.detail}</pre>}
      {result.checks.map((check, at) => {
        const target = targets[at]
        return (
          <CheckCard
            key={at}
            check={check}
            sides={view.sides}
            saidLabel={view.saidLabel}
            said={<LogLine result={result} log={said} />}
            onAnchor={target === undefined ? undefined : () => show(target)}
          />
        )
      })}
      {result.checks.length === 0 && result.outcome !== "error" && (
        <p className="mono">
          {view.saidLabel}
          <LogLine result={result} log={said} />
        </p>
      )}
      {cell !== null && attached.state === "error" && <p className="cp-nok">attachments: {attached.message}</p>}
      {cell !== null && attached.state === "ok" && <AttachmentList listed={listed} heading="Attachments" />}
      {cell !== null && (
        <CollectOnDemand
          key={`${laneRun.runId}/${cell}`}
          cell={collectCellOf(capture, laneRun, cell, caseId)}
          onLanded={() => setLanded((n) => n + 1)}
        />
      )}
      {cell !== null && <RecordsCompare view={view} caseId={caseId} run={run} lane={lane} listed={listed} />}
      {cell !== null && detail.state === "loading" && <p className="muted">…</p>}
      {detail.state === "error" && <p className="cp-nok">{detail.message}</p>}
      {layout !== undefined && (
        <>
          <div ref={ladderRef}>
            <LadderPane
              layout={layout}
              heading="Ladder"
              selectedId={selected}
              onSelect={(event) => setSelected(event.id)}
              legend={false}
              detail={picked === undefined ? undefined : <SipMessageView record={picked.source} label={picked.id} />}
            />
          </div>
          <h3>HTTP</h3>
          <Exchange
            exchanges={http.exchanges}
            warnings={http.warnings}
            ladder={{ after, selectedId: selected, onSelect: show }}
          />
        </>
      )}
    </>
  )
}

/** The copy control of the modal's own link. */
const CopyLink = ({ url }: { readonly url: string }): JSX.Element => {
  const copy = useCopy()
  return (
    <button type="button" className="copy-ref" title={url} onClick={() => copy.copy(url)}>
      {copy.label("copy link")}
    </button>
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
  const origin = useLinkOrigin(window.location.origin)
  const link = caseLink(origin, view.capture, { env: view.env ?? undefined, caseId, run, lane })
  return (
    <div className="cp-modal" onClick={(event) => event.target === event.currentTarget && onClose()}>
      <div className="cp-modal-body">
        <button type="button" className="cp-close" onClick={onClose} title="close (Esc)">✕</button>
        <h2>Call details</h2>
        <p className="muted mono">#{run} · {lane} · {caseId} <CopyLink url={link} /></p>
        {laneRun === undefined || result === undefined ? <p className="muted">— not run</p> : (
          <CallDetails view={view} laneRun={laneRun} result={result} caseId={caseId} run={run} lane={lane} />
        )}
      </div>
    </div>
  )
}

/**
 * One capture's study view: everything a triage session reads, on one
 * shareable URL — the capture ladder, the case ladder cut from it and the
 * replay ladder side by side, the confrontation records classified, the
 * registry verdict, and the paths back to every artifact. The URL carries the
 * capture basename, so it is itself a valid triage launch ref; its query
 * carries the replay on screen and the step, so a finding has a link of its
 * own. With `case` the page is the study of that ONE call: the server answers
 * the capture's envelope over that call's cells, case and refusal alone, the
 * head names the call and links up to the capture study, and a refused call
 * shows its own refusal record over the legs it was judged on.
 *
 * The page leads with the RUN'S OWN VERDICT. The delta classification below it
 * rules on wire differences only, and a replay that stopped early diverges from
 * almost nothing — so the two must never be confused, and the verdict is what
 * says whether there is a defect here at all.
 *
 * The page renders whatever exists: a capture with no session yet shows its
 * flows ladder and cut cases with the launch command; a capture the cut refused
 * shows the refusal; the ladders and confrontation appear per replay cell once
 * a session ran.
 *
 * The study lists what every cell RULED; the evidence of the one on screen is
 * fetched for it. A capture swept hundreds of times would otherwise ship every
 * recording it ever produced to draw one of them.
 */
import {
  buildFlowsLayout,
  buildRecordingLayout,
  buildScenarioLayout,
  flowsToText,
  ladderToText,
  NO_HTTP_EXCHANGES,
  parseFlowsDocument,
  parseRecording,
  type FlowsRow,
  type LadderEvent
} from "@sip/viewer-core"
import type { JSX, ReactNode } from "react"
import { useEffect, useMemo, useRef, useState } from "react"
import { api, useAsync, type CaseRun, type TriageRefusal, type TriageStudy, type TriageStudyCell } from "../api.ts"
import { useCaptureFlows } from "../capture-flows.ts"
import { useHeld } from "../held.ts"
import { laneOf, rememberLane } from "../lane-memory.ts"
import { Age } from "../components/Age.tsx"
import { useCaptureFilters } from "../components/capture-filters.ts"
import { CallLanes } from "../components/CallLanes.tsx"
import { Confrontation } from "../components/Confrontation.tsx"
import { CopyChip } from "../components/CopyChip.tsx"
import { CopyText } from "../components/CopyText.tsx"
import { DetailChips, scalarFields } from "../components/DetailChips.tsx"
import { Deltas, deltasOfClassification, FailedSteps } from "../components/Deltas.tsx"
import { ExclusionPane } from "../components/ExclusionPane.tsx"
import { Exchange } from "../components/Exchange.tsx"
import { annotationsOf, Flags } from "../components/Flags.tsx"
import { FlowsPane } from "../components/FlowsPane.tsx"
import { IgnoreRecord } from "../components/IgnoreRecord.tsx"
import { JsonView } from "../components/JsonView.tsx"
import { LadderPane } from "../components/LadderPane.tsx"
import { Profile } from "../components/Profile.tsx"
import { Link } from "../components/Link.tsx"
import { ReplayButton } from "../components/ReplayButton.tsx"
import { RfcAuditPane } from "../components/RfcAuditPane.tsx"
import { useDock } from "../components/dock.ts"
import { type RowAnchor, RowAnchorContext } from "../components/scroll.ts"
import { SipMessageView } from "../components/SipMessageView.tsx"
import { StudyHead } from "../components/StudyHead.tsx"
import { SutRecords } from "../components/SutRecords.tsx"
import { VerdictPane } from "../components/VerdictPane.tsx"
import { findingsByStep, marksOnLadder } from "../findings.ts"
import { caseIdOf, cellPassed, studyCells } from "../study-cells.ts"
import { useCommands } from "../commands.ts"
import { launchRef } from "../launch.ts"
import { cellDocument, cellOpening, NO_OPENING, openingOfExclusions, type CaptureOpening } from "../opening.ts"
import { href, setQuery, useQuery } from "../router.ts"
import { captureSteps, focusStep, recordingSteps, scenarioSteps, stepOfEvent, type Focus } from "../steps.ts"

/** A ladder about no leg in particular: the capture's own first legs are drawn. */
const NO_LEGS: readonly number[] = []

/**
 * How ONE call ended, in a word. A run that mixes a green call and a red one
 * has no single outcome, so nothing on this page states one: every verdict is
 * a call's own.
 */
const cellState = (cell: TriageStudyCell): { label: string; tone: string } => {
  if (cell.refused) return { label: "refused", tone: "missing" }
  if (cell.skipped !== undefined) return { label: "skipped", tone: "muted" }
  return cellPassed(cell) ? { label: "passed", tone: "matched" } : { label: "failed", tone: "missing" }
}

/** The launch ref of what the page is OF: the one call where it has one, else the capture. */
const CopyRef = (
  { capture, lane, caseId }: { capture: string; lane: string | undefined; caseId: string | undefined }
): JSX.Element | null => {
  const [copied, setCopied] = useState(false)
  const command = useCommands().triage
  if (command === undefined) return null
  const ref = launchRef(command, capture, lane, caseId)
  return (
    <button
      type="button"
      className="copy-ref"
      title={`copy "${ref}"`}
      onClick={() => {
        void navigator.clipboard.writeText(ref).then(() => {
          setCopied(true)
          window.setTimeout(() => setCopied(false), 1200)
        })
      }}
    >
      {copied ? "copied" : "copy ref"}
    </button>
  )
}

/**
 * The cell's outcome in one line: what the run decided, then what the deltas
 * cost. The verdict comes first and is never softened by the classification —
 * a failed run under a clean delta sheet is exactly the case this ordering
 * exists for.
 */
const Outcome = (
  { cell, classification, onPickStep }: {
    cell: TriageStudyCell
    /** The cell's `classification.json`; absent until its evidence lands. */
    classification: unknown
    onPickStep: (step: string) => void
  }
): JSX.Element => {
  const deltas = deltasOfClassification(classification)
  return (
    <p className="counts outcome">
      <span className={`tag ${cellPassed(cell) ? "matched" : "missing"}`}>
        {cell.status ?? "no verdict"}
      </span>
      <Age at={cell.writtenAt} prefix="replayed" />
      {cell.failedStep !== undefined && (
        <FailedSteps steps={[cell.failedStep]} onPick={onPickStep} />
      )}
      <span className="muted">deltas</span>
      {deltas === undefined
        ? <span className="muted">none</span>
        : <Deltas counts={deltas} detail />}
    </p>
  )
}

/** The evidence panes, and which of them are open. */
const PANES = [
  { key: "capture", label: "capture" },
  { key: "case", label: "case" },
  { key: "replay", label: "replay" },
  { key: "http", label: "http" }
] as const

type PaneKey = typeof PANES[number]["key"]

/**
 * One ladder pane of the study: a one-line head (title, path, copy, hide),
 * then the pane's ladder in its three slots, lined up with the other panes'
 * by the grid they share.
 */
const Pane = (
  { title, path, text, onHide, children }: {
    title: string
    path?: string | undefined
    /** The ladder as text, for the head's copy button. */
    text?: (() => string) | undefined
    onHide: () => void
    children: ReactNode
  }
): JSX.Element => (
  <section className="study-pane">
    <header className="study-pane-head">
      <h3>{title}</h3>
      {path !== undefined && <CopyChip label="path" value={path} />}
      <span className="study-pane-actions">
        {text !== undefined && <CopyText text={text} label="copy text" />}
        <button type="button" className="copy-ref" title="hide this pane" onClick={onHide}>×</button>
      </span>
    </header>
    {children}
  </section>
)

/** The detail slot of a pane that holds no selected message while another pane does. */
const NoDetail = ({ step }: { step: string | undefined }): JSX.Element => (
  <p className="muted detail-empty">{step === undefined ? "—" : `${step}: not on this ladder`}</p>
)

/**
 * This call alone: its own last run where it is not the run on screen, and
 * the button that replays it again — a job, linked; the page is reloaded to
 * read the run it lands. The capture's standing is the header's; nothing
 * here moves it.
 */
const OwnRun = (
  { study, cell, session, lane }: {
    study: TriageStudy
    cell: TriageStudyCell
    /** The run on screen, `run<id>`. */
    session: string
    lane: string | undefined
  }
): JSX.Element => {
  const caseId = caseIdOf(cell.name)
  const own: CaseRun | undefined = study.standing?.caseRuns.find((entry) => entry.caseId === caseId)
  const elsewhere = own !== undefined && `run${own.runId}` !== session
  return (
    <p className="counts own-run">
      <span className="muted">this call</span>
      {elsewhere && (
        <span className={`tag ${own.skipped !== null ? "muted" : own.passed ? "matched" : "missing"}`}>
          run{own.runId} {own.skipped !== null ? "skipped" : own.passed ? "passed" : "failed"}
        </span>
      )}
      <ReplayButton capture={study.capture} lane={lane ?? ""} caseId={caseId} />
    </p>
  )
}

const CellStudy = (
  { study, cell, session, step, onStep, lane }: {
    study: TriageStudy
    cell: TriageStudyCell
    /** The run the cell belongs to: the other half of its address. */
    session: string
    /** The step the URL opened on, or undefined. */
    step: string | undefined
    onStep: (step: string | undefined) => void
    /** The lane the page reads on: what a replay of this call alone asks. */
    lane: string | undefined
  }
): JSX.Element => {
  const [focus, setFocus] = useState<Focus>({})
  // The capture pane opens on THIS call's legs alone: on a capture of
  // thousands of calls the window around them is other calls' traffic, which
  // the reader extends to on purpose, never by default.
  const [onlySelected, setOnlySelected] = useState(true)
  const filters = useCaptureFilters()
  const [shown, setShown] = useState<ReadonlySet<PaneKey>>(
    new Set(["capture", "case", "replay", "http"])
  )

  // The evidence — the document, the recording, the delta sheet — is fetched
  // for the cell ON SCREEN, never listed: a capture gains a cell per sweep, and
  // a hundred of them is megabytes the page draws one of.
  const evidence = useAsync(
    () => api.triageCell(study.capture, session, cell.name),
    `${study.capture}/${session}/${cell.name}`
  )
  const detail = evidence.state === "ok" ? evidence.value : undefined

  const toggle = (key: PaneKey): void =>
    setShown((current) => {
      const next = new Set(current)
      if (!next.delete(key)) next.add(key)
      return next
    })

  // A skipped cell has no `pivot.json` — nothing ran, so the bundle holds no
  // document. The case it WOULD have replayed is the standing cut's, and it is
  // what the capture and case panes are read for; a cell with no document at
  // all — of a call the cut since refused — is drawn from that call's own
  // exclusion record instead (`../opening.ts`).
  const skipped = cell.skipped
  const studyCase = study.cases.find((entry) => cell.name.startsWith(`${entry.id}__`))
  const pivotRaw: unknown = detail?.pivot ?? studyCase?.document
  const pivotRead = useMemo(() => cellDocument(detail?.pivot, studyCase?.document), [detail, studyCase])
  const opening = useMemo(
    () => cellOpening(pivotRead, study.exclusions, caseIdOf(cell.name)),
    [pivotRead, study.exclusions, cell.name]
  )
  const selection = opening.selection
  // The capture ladder opens on the legs THIS case was cut from, and reads
  // them a window at a time: a capture of thousands of calls is a document no
  // response carries whole.
  const caseLegs = useMemo(() => [...(selection?.legs ?? [])], [selection])
  const flows = useCaptureFlows(study.capture, study.flows, caseLegs)
  const flowsRead = useMemo(
    () => (flows.document === undefined ? undefined : parseFlowsDocument(flows.document)),
    [flows.document]
  )
  const flowsLayout = useMemo(() => {
    if (flowsRead === undefined || !flowsRead.ok) return undefined
    return buildFlowsLayout(flowsRead.document, {
      selection,
      sut: opening.sut,
      onlySelected,
      legs: filters.legs.value,
      sockets: filters.sockets.value,
      title: study.capture
    })
  }, [flowsRead, opening, study, onlySelected, filters.legs.value, filters.sockets.value])
  const runLayout = useMemo(() => {
    const recording = parseRecording(detail?.recording ?? {})
    return buildRecordingLayout(recording, {
      document: pivotRead.ok ? pivotRead.document : undefined,
      title: cell.name,
      subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages`
    })
  }, [cell, detail, pivotRead])
  // What the SUT asked its HTTP service during this very run, beside the SIP it decided.
  const served = detail?.http ?? NO_HTTP_EXCHANGES
  const caseLayout = useMemo(
    () => (pivotRead.ok ? buildScenarioLayout(pivotRead.document) : undefined),
    [pivotRead]
  )

  const indexes = useMemo(() => ({
    capture: captureSteps(flowsLayout),
    replay: recordingSteps(runLayout),
    scenario: scenarioSteps(caseLayout)
  }), [flowsLayout, runLayout, caseLayout])

  // What the run RULED, on the rows it ruled it at. The failure is a property
  // of one step, so it belongs on that step's arrow and not only in a pane the
  // reader has to scroll back up to.
  const findings = useMemo(() => findingsByStep(detail?.verdict, detail?.confrontation), [detail])
  const replayMarks = useMemo(() => marksOnLadder(findings, indexes.replay), [findings, indexes])
  const caseMarks = useMemo(() => marksOnLadder(findings, indexes.scenario), [findings, indexes])

  // The ladders side by side share one anchor: a row clicked on one ladder is
  // put at the same height on the others (`scroll.ts`).
  const anchor = useMemo<RowAnchor>(() => ({ current: undefined }), [])

  // The URL's step is a selection like any other, applied once the layouts that
  // resolve it exist. It only ever ADDS a focus: a click on a message no step
  // cites must not be undone by the URL saying nothing.
  useEffect(() => {
    if (step === undefined) return
    if (anchor.current === undefined) anchor.current = "centre"
    setFocus(focusStep(step, indexes))
  }, [step, indexes, anchor])

  // Every pick off the ladders docks the workbench, so the rows it selects are
  // on screen; the step a link opened on docks it once its evidence is in.
  const workbench = useDock()
  const [openedOnStep] = useState(step !== undefined)
  const dockedOnLoad = useRef(false)
  useEffect(() => {
    if (!openedOnStep || dockedOnLoad.current || evidence.state !== "ok") return
    dockedOnLoad.current = true
    workbench.dock()
  }, [openedOnStep, evidence.state, workbench])

  const pickStep = (next: string): void => {
    anchor.current = "centre"
    setFocus(focusStep(next, indexes))
    onStep(next)
    workbench.dock()
  }
  const clearStep = (): void => {
    setFocus({})
    onStep(undefined)
  }
  // Clicking the message already selected UNSELECTS it, on every pane at once:
  // three detail panels open are most of the viewport spent on a message the
  // reader has finished with.
  const pickCapture = (row: FlowsRow): void => {
    if (focus.capture === row.id) return clearStep()
    setFocus({ ...(row.stepId === undefined ? {} : focusStep(row.stepId, indexes)), capture: row.id })
    onStep(row.stepId)
  }
  const pickReplay = (event: LadderEvent): void => {
    if (focus.replay === event.id) return clearStep()
    const eventStep = stepOfEvent(event)
    setFocus({ ...(eventStep === undefined ? {} : focusStep(eventStep, indexes)), replay: event.id })
    onStep(eventStep)
  }
  const pickCase = (event: LadderEvent): void => {
    if (focus.scenario === event.id) return clearStep()
    const eventStep = stepOfEvent(event)
    setFocus({ ...(eventStep === undefined ? {} : focusStep(eventStep, indexes)), scenario: event.id })
    onStep(eventStep)
  }

  const captureSelected = flowsLayout?.rows.find((row) => row.id === focus.capture)
  const replaySelected = runLayout.events.find((event) => event.id === focus.replay)
  const caseSelected = caseLayout?.events.find((event) => event.id === focus.scenario)
  // Every pane holds a detail box while any of them holds a message, so the
  // boxes and the ladders under them start on one line.
  const anyDetail = captureSelected !== undefined || replaySelected !== undefined || caseSelected !== undefined
  const placeholder = anyDetail ? <NoDetail step={focus.step} /> : undefined
  // The column count is the LADDER panes': the http pane is a section of its own.
  const open = PANES.filter((pane) => shown.has(pane.key) && pane.key !== "http")

  return (
    <>
      <div className="study-verdict">
        <Outcome cell={cell} classification={detail?.classification} onPickStep={pickStep} />
        <OwnRun study={study} cell={cell} session={session} lane={lane} />
        {skipped === undefined
          ? (
            <>
              <VerdictPane verdict={detail?.verdict} rfc={detail?.rfc} onPickStep={pickStep} />
              <RfcAuditPane rfc={detail?.rfc} onPickStep={pickStep} layout={runLayout} />
            </>
          )
          : (
            <p className="skipped-note" title="the document declares this lane blocked: no verdict, no replay">
              <strong>skipped</strong> · lane <code>{skipped}</code> blocked
            </p>
          )}
        {/* A cell with no document of a call the standing cut refused: the
            record is the verdict, and the capture pane below is drawn from it. */}
        {!pivotRead.ok && study.exclusions !== undefined && opening.selection !== undefined && (
          <ExclusionPane exclusion={study.exclusions} caseId={caseIdOf(cell.name)} />
        )}
        {cell.refused && (
          <div className="refused-note">
            <h3>interpreter refused this case</h3>
            <pre className="raw">{detail?.error ?? "loading…"}</pre>
          </div>
        )}
        {detail?.stderr !== undefined && (
          <details className="study-record cell-stderr">
            <summary>
              stderr <span className="count">{detail.stderr.split("\n").filter((one) => one !== "").length} lines</span>
            </summary>
            <pre className="raw">{detail.stderr}</pre>
          </details>
        )}
        {detail?.join !== undefined && detail.join !== null && (
          <SutRecords join={detail.join} windows={detail.windows ?? []} held={detail.held ?? []} />
        )}
        {evidence.state === "error" && <p className="error">evidence: {evidence.message}</p>}
        {detail?.confrontationProblem !== undefined && (
          <p className="error">confrontation: {detail.confrontationProblem}</p>
        )}
      </div>

      <RowAnchorContext.Provider value={anchor}>
        <section
          ref={workbench.ref}
          className="study-workbench"
          data-detail={anyDetail}
          data-open={open.length}
          data-docked={workbench.docked}
        >
          <div className="pane-toolbar">
            {PANES.map((pane) => (
              <label key={pane.key}>
                <input type="checkbox" checked={shown.has(pane.key)} onChange={() => toggle(pane.key)} />
                {pane.label}
              </label>
            ))}
            {focus.step !== undefined && (
              <span className="focus-note">
                step <code>{focus.step}</code>
                <button type="button" className="copy-ref" onClick={clearStep}>clear</button>
              </span>
            )}
          </div>

          {open.length > 0 && (
            <div className="study-panes">
              {shown.has("capture") && (
                <Pane
                  title="capture"
                  path={study.flows?.path}
                  text={flowsLayout === undefined ? undefined : () => flowsToText(flowsLayout)}
                  onHide={() => toggle("capture")}
                >
                  {study.flows === undefined && <p className="muted">no flows document</p>}
                  {flows.error !== undefined && <p className="error">flows: {flows.error}</p>}
                  {flowsRead !== undefined && !flowsRead.ok && <p className="error">flows: {flowsRead.reason}</p>}
                  {flowsLayout && (
                    <FlowsPane
                      layout={flowsLayout}
                      selected={captureSelected}
                      onSelect={pickCapture}
                      onlySelected={{ value: onlySelected, onChange: setOnlySelected }}
                      legs={filters.legs}
                      sockets={filters.sockets}
                      tag={filters.tag}
                      window={{ ...flows.window, beyond: flows.beyond, total: study.flows?.legs ?? 0, onWindow: flows.onWindow }}
                      copy={false}
                      compact
                      placeholder={placeholder}
                    />
                  )}
                </Pane>
              )}
              {shown.has("case") && (
                <Pane
                  title="case"
                  path={`${cell.dir}/pivot.json`}
                  text={caseLayout === undefined ? undefined : () => ladderToText(caseLayout)}
                  onHide={() => toggle("case")}
                >
                  {evidence.state === "loading" && pivotRaw === undefined
                    ? <p className="muted">loading…</p>
                    : !pivotRead.ok
                    ? <p className="error">case document: {pivotRead.reason}</p>
                    : caseLayout && (
                      <LadderPane
                        layout={caseLayout}
                        selectedId={focus.scenario}
                        onSelect={pickCase}
                        marksById={caseMarks}
                        legend={false}
                        copy={false}
                        compact
                        controls={
                          <>
                            <details className="pane-fold">
                              <summary>flags</summary>
                              <Flags annotations={annotationsOf(pivotRaw)} />
                            </details>
                            <details className="pane-fold">
                              <summary>pivot.json</summary>
                              <JsonView value={pivotRaw} label="pivot.json" />
                            </details>
                          </>
                        }
                        detail={caseSelected === undefined
                          ? placeholder
                          : (
                            <div className="json-detail">
                              <DetailChips fields={scalarFields(caseSelected.source)} />
                              <JsonView value={caseSelected.source} label={`case · step ${caseSelected.id}`} />
                            </div>
                          )}
                      />
                    )}
                </Pane>
              )}
              {shown.has("replay") && (
                <Pane
                  title="replay"
                  path={cell.dir}
                  text={() => ladderToText(runLayout)}
                  onHide={() => toggle("replay")}
                >
                  {evidence.state === "loading"
                    ? <p className="muted">loading…</p>
                    : (
                      <LadderPane
                        layout={runLayout}
                        selectedId={focus.replay}
                        onSelect={pickReplay}
                        marksById={replayMarks}
                        legend={false}
                        copy={false}
                        compact
                        detail={replaySelected === undefined
                          ? placeholder
                          : <SipMessageView record={replaySelected.source} label={`replay · ${replaySelected.id}`} />}
                      />
                    )}
                </Pane>
              )}
            </div>
          )}
        </section>
      </RowAnchorContext.Provider>

      {shown.has("http") && (
        <section className="panel study-http">
          <header className="study-pane-head">
            <h3>http</h3>
            <CopyChip label="path" value={`${cell.dir}/http.json`} />
            <span className="study-pane-actions">
              <button type="button" className="copy-ref" title="hide this pane" onClick={() => toggle("http")}>×</button>
            </span>
          </header>
          <Exchange exchanges={served.exchanges} warnings={served.warnings} />
          {/* What the mock was SET to answer, as the cut wrote it: what the
              exchange above should be read against. */}
          <details className="study-record">
            <summary>mock profile</summary>
            <Profile ledger={studyCase?.ledger} />
          </details>
        </section>
      )}

      <section className="panel confrontation-section">
        <h3>
          confrontation{" "}
          <span className="count">{evidence.state === "loading" ? "…" : (detail?.confrontation ?? []).length}</span>
        </h3>
        {evidence.state === "loading"
          ? <p className="muted">loading…</p>
          : (detail?.confrontation ?? []).length === 0
          ? <p className="muted">none</p>
          : (
            <Confrontation
              rows={detail?.confrontation ?? []}
              focusedStep={focus.step}
              onPickStep={pickStep}
            />
          )}
        <details className="study-record">
          <summary>verdict.json</summary>
          <JsonView value={detail?.verdict} label="verdict.json" />
        </details>
        {detail !== undefined && detail.terminated !== null && (
          <details className="study-record">
            <summary>call.terminated.json</summary>
            <JsonView value={detail.terminated} label="call.terminated.json" />
          </details>
        )}
      </section>
    </>
  )
}

interface Call {
  readonly caseId: string
  readonly key: string
  readonly label: string
  readonly tone: string
  readonly failedStep: string | undefined
}

/**
 * One link per call the capture produced, each stating its OWN outcome, each
 * opening the capture study on that call's replay.
 *
 * A pcap that cut several calls asked several independent questions: one of
 * them going red says nothing about the others, so there is no run-level
 * verdict anywhere on this page — the chips are it, and each is an address.
 */
const CallBar = (
  { calls, activeCase, capture, lane }: {
    calls: readonly Call[]
    activeCase: string | undefined
    capture: string
    /** The lane the study was read on; every chip opens on the same one. */
    lane: string | undefined
  }
): JSX.Element => (
  <p className="call-bar">
    <span className="muted">calls <span className="count">{calls.length}</span></span>
    {calls.map((call) => (
      <Link
        key={call.key}
        to={href.triageStudy(capture, {
          lane,
          cell: call.key,
          ...(call.failedStep === undefined ? {} : { step: call.failedStep })
        })}
        className={`call-chip${call.caseId === activeCase ? " on" : ""}`}
      >
        <span className={`tag ${call.tone}`}>{call.label}</span>
        {call.caseId.replace(capture, "…")}
        {call.failedStep !== undefined && <span className="call-step">✗ {call.failedStep}</span>}
      </Link>
    ))}
  </p>
)

/**
 * The standing cut's refusal of the one call on screen: every reason, the
 * calls it was charged on, the cut's own line. There is no cell under it and
 * none is implied; the generator's record and the capture below are the rest.
 */
const RefusalNote = ({ refusal }: { refusal: TriageRefusal }): JSX.Element => (
  <section className="refused-note">
    <h3>refused by the cut</h3>
    <p className="counts">
      {refusal.reasons.map((reason) => <span key={reason} className="tag error-refused">{reason}</span>)}
    </p>
    {refusal.callIds.length > 0 && (
      <p className="muted">
        Call-IDs{" "}
        {refusal.callIds.map((callId, index) => (
          <span key={callId}>
            {index > 0 && " "}
            <code>{callId}</code>
          </span>
        ))}
      </p>
    )}
    {refusal.detail !== "" && <p className="exclusion-line">{refusal.detail}</p>}
  </section>
)


export const StudyPage = ({ capture }: { capture: string }): JSX.Element => {
  const query = useQuery()
  const commands = useCommands()
  // Which lane's standing this capture is read against: a study opened from the
  // real lane's queue must not report the fake lane's run, and one opened by name
  // reads the lane the browser remembers. The lane the URL carried when the
  // page opened is remembered, as on the pickers' pages; the lane the page's
  // own rewrite states below is the server's answer, and is never remembered.
  const [openedOn] = useState(() => query.get("lane"))
  useEffect(() => {
    if (openedOn !== null && openedOn !== "") rememberLane(openedOn)
  }, [openedOn])
  const queryLane = laneOf(query) || undefined
  const queryCell = query.get("cell") ?? undefined
  // The cell the link was opened on: a cited one whose run is gone is said so,
  // even once the page has moved to a cell it holds.
  const [citedCell] = useState(queryCell)
  // `case` makes the page the study of that one call; without it, the capture's.
  const queryCase = query.get("case") ?? undefined
  const queryStep = query.get("step") ?? undefined
  // The last payload is held across a reload: blanking the page for the fetch
  // unmounts the re-run button mid-job and throws away what it had to report.
  const study = useHeld(
    () => api.triageStudy(capture, queryLane, queryCase),
    `${capture}#${queryLane ?? ""}#${queryCase ?? ""}`
  )
  const loaded = study.current
  const [cellKey, setCellKey] = useState<string | undefined>(queryCell)
  const [step, setStep] = useState<string | undefined>(queryStep)

  // A link into this page carries the call and the step; a link followed while
  // the page is already mounted must move it, which only the query says.
  useEffect(() => setCellKey(queryCell), [queryCell])
  useEffect(() => setStep(queryStep), [queryStep])

  // The lane every reading and every action on this page stays on: what the run
  // database answered for, else what the URL asked for.
  const lane = loaded?.standing?.lane ?? queryLane
  // Which cells the page lists and opens on, a run set aside opened only by its link (`study-cells.ts`).
  const { active, cells, latest, picker } = studyCells(loaded?.sessions ?? [], lane, cellKey, queryCase)
  const activeCase = active === undefined ? undefined : caseIdOf(active.cell.name)
  // The header follows the CELL on screen: picking an older run must not leave
  // the page stating the latest run's number over an older run's ladders.
  const activeSession = active?.session
  // Hooks and held state come from the cell's run; with no cell on screen, from the latest run not set aside.
  const shownSession = activeSession ?? [...(loaded?.sessions ?? [])].reverse().find((one) => one.aside === undefined)
  // The prep the run on screen read its cases as of, where it is the standing
  // run and that prep is not the standing cut's: the lanes pin their own preps.
  const runPrep = loaded?.standing !== undefined
      && (activeSession === undefined || activeSession.name === `run${loaded.standing.runId}`)
      && loaded.standing.prepId !== null
      && loaded.standing.prepId !== loaded.cut?.prepId
    ? loaded.standing.prepId
    : undefined

  // The URL always states where the page is, including the cell it defaulted
  // to: what makes the address bar worth copying is that nobody had to click.
  // `case` stays exactly as the URL had it: it is what the page is OF.
  useEffect(() => {
    if (active === undefined) return
    setQuery({ lane, case: queryCase, cell: active.key, step })
  }, [active?.key, queryCase, step, lane])

  if (loaded === undefined) {
    if (study.error !== undefined) return <p className="page error">{study.error}</p>
    return <p className="page muted">loading {capture}…</p>
  }

  const value = loaded
  const onDisk = (key: string): boolean =>
    value.sessions.some((session) => session.cells.some((cell) => `${session.name}/${cell.name}` === key))
  const citedGone = citedCell !== undefined && !onDisk(citedCell)
    ? /^run(\d+)\//.exec(citedCell)?.[1]
    : undefined
  const calls: readonly Call[] = [...new Map(
    latest.map((entry) => [caseIdOf(entry.cell.name), {
      caseId: caseIdOf(entry.cell.name),
      key: entry.key,
      ...cellState(entry.cell),
      failedStep: entry.cell.failedStep
    }])
  ).values()]

  return (
    <div className="page study-page">
      <header className="page-head">
        <div>
          <StudyHead study={value} lane={lane} cell={active?.key} />
          {/* The run's identity and its age — never a verdict: a run over
              several calls has none, and each chip below states its own. */}
          <p className="muted study-run">
            {study.loading && <span className="tag">reloading…</span>}
            {study.error !== undefined && <span className="tag missing">{study.error}</span>}{" "}
            {activeSession?.aside !== undefined && (
              <>
                <span className="tag">{activeSession.name}: {activeSession.aside}, set aside</span>{" "}
              </>
            )}
            {value.standing === undefined
              ? activeSession === undefined
                ? `never run on ${queryLane ?? "the configured lane"}`
                : (
                  // No capture-wide standing on this lane, but the call on screen ran on it.
                  <>
                    {lane ?? queryLane} · {activeSession.name} · <Age at={activeSession.writtenAt} />
                  </>
                )
              : (
                <>
                  {value.standing.lane} · {activeSession?.name ?? `run${value.standing.runId}`}
                  {runPrep !== undefined && ` · prep${runPrep}`} ·{" "}
                  <Age at={activeSession?.writtenAt ?? value.standing.finishedAt ?? undefined} />
                  {value.standing.lastOkRunId === null
                    ? " · never green"
                    : ` · last green run${value.standing.lastOkRunId}`}
                </>
              )}
            {value.ticket && (
              <>
                {" "}· ticket {value.ticket.ref}{" "}
                {value.ticket.unavailable === undefined
                  ? <span className={`tag ticket-${value.ticket.state}`}>{value.ticket.state}</span>
                  : <span className="tag muted">{value.ticket.unavailable}</span>}
              </>
            )}
          </p>
          {/* Where the cases and the flows doc come from, one line of chips.
              The lanes pin their own preps, so the standing cut and the run
              that produced the ladders are stated apart. */}
          <p className="muted study-paths">
            {value.cut !== undefined && (
              <span className="mono">
                cut prep{value.cut.prepId} · {value.cut.cases} cases · <Age at={value.cut.cutAt} />
                {value.cut.broke !== null && <span className="error"> · broke: {value.cut.broke}</span>}
              </span>
            )}
            {value.corpusDir !== undefined && <CopyChip label="corpus" value={value.corpusDir} />}
            {value.sessionDir !== undefined && <CopyChip label="session" value={value.sessionDir} />}
            {value.flows?.path !== undefined && <CopyChip label="flows" value={value.flows.path} />}
          </p>
          {/* What the deployment's hooks recorded around the run on screen —
              the box's own account of that replay, kept beside it. */}
          {shownSession?.hooks.map((hook) => (
            <details key={hook.name} className="rerun-log study-hook">
              <summary className="muted">
                hook {hook.name}
                {hook.bytes !== undefined && <span className="count">{hook.bytes} B</span>}
              </summary>
              {hook.href !== undefined && <p className="muted"><a href={hook.href}>whole log</a></p>}
              <pre className="raw">{hook.text}</pre>
            </details>
          ))}
          {/* What each batch's cleanup gate read of the SUT's call state:
              what it held before and after, and what the batch's calls left. */}
          {(shownSession?.held ?? []).length > 0 && (
            <details className="rerun-log study-held">
              <summary className="muted">SUT call state</summary>
              <JsonView value={shownSession?.held} label="contexts" />
            </details>
          )}
          {value.case !== undefined && <CallLanes capture={value.capture} caseId={value.case} lane={lane} />}
        </div>
        <div className="page-actions">
          <CopyRef capture={value.capture} lane={lane} caseId={value.case} />
          {/* The run on screen as one plain text: ladders, hook logs and the
              deployment's trace sources, for a ticket or an agent. `full`
              unfolds every SIP body and every HTTP exchange with it; the study
              of one call exports that call's cells alone. */}
          {activeSession !== undefined && ([["text", ""], ["full text", "full=1"]] as const).map(
            ([label, mode]) => {
              const query = [
                ...(lane === undefined ? [] : [`lane=${encodeURIComponent(lane)}`]),
                ...(value.case === undefined ? [] : [`case=${encodeURIComponent(value.case)}`]),
                mode
              ]
                .filter((part) => part !== "")
                .join("&")
              return (
                <a
                  key={label}
                  className="copy-ref"
                  href={`/api/triage/${encodeURIComponent(value.capture)}/runs/${activeSession.name}` +
                    `/text${query === "" ? "" : `?${query}`}`}
                  target="_blank"
                  rel="noreferrer"
                >
                  {label}
                </a>
              )
            }
          )}
          <ReplayButton capture={value.capture} lane={lane ?? ""} />
          {picker.length > 1 && (
            <select
              value={active?.key ?? ""}
              onChange={(event) => setCellKey(event.target.value)}
            >
              {picker.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.key}
                  {cellPassed(entry.cell) ? " · passed" : " · failed"}
                </option>
              ))}
            </select>
          )}
        </div>
      </header>

      {calls.length > 0 && value.case === undefined && (
        <CallBar calls={calls} activeCase={activeCase} capture={value.capture} lane={lane} />
      )}

      {/* A cut that currently refuses every call, said BEFORE any ladder: the
          bundles below it, where there are any, belong to an older cut. */}
      {value.standing !== undefined && value.standing.cases === 0 && value.exclusions !== undefined && (
        <section className="refused-note">
          <h3>refused by the current cut</h3>
          <p className="muted" title="any ladder below comes from an earlier cut">
            run{value.standing.runId}: no case
          </p>
          <ExclusionPane exclusion={value.exclusions} />
        </section>
      )}

      {value.record !== null && (
        <details className="study-record" open={false}>
          <summary>registry record</summary>
          <JsonView value={value.record} label="registry" />
        </details>
      )}

      {/* The shortcut out: a capture whose source is plainly unreplayable is a
          one-sentence verdict, and one already recorded needs no second. */}
      {value.record === null && (
        <IgnoreRecord
          capture={value.capture}
          repoRoot={value.repoRoot}
          evidenceDir={active?.cell.dir ?? value.sessionDir ?? value.corpusDir}
          step={active?.cell.failedStep}
        />
      )}

      {citedGone !== undefined && (
        <CitedRunGone capture={value.capture} runId={Number(citedGone)} lane={lane} caseId={queryCase} cell={citedCell!} />
      )}
      {active !== undefined
        ? (
          <CellStudy
            key={active.key}
            study={value}
            cell={active.cell}
            session={active.session.name}
            step={step}
            onStep={setStep}
            lane={lane}
          />
        )
        : value.refusal !== undefined
        ? (
          <section>
            <RefusalNote refusal={value.refusal} />
            {value.exclusions !== undefined && <ExclusionPane exclusion={value.exclusions} />}
            <StandaloneCapture study={value} opening={openingOfExclusions(value.exclusions)} />
          </section>
        )
        : (
          <section>
            {value.exclusions === undefined
              ? (
                <p className="muted">
                  no replay yet
                  {commands.triage !== undefined && (
                    <> · <code>{launchRef(commands.triage, value.capture, lane, value.case)}</code></>
                  )}
                </p>
              )
              : <ExclusionPane exclusion={value.exclusions} />}
            {value.cases.length === 0
              ? <p className="muted">no cases</p>
              : (
                <ul className="mono">
                  {value.cases.map((entry) => <li key={entry.id}>{entry.id}</li>)}
                </ul>
              )}
            {/* A capture the cut made no case of has only its refusals to draw;
                beside cut cases, the records are other calls' and the ladder
                opens on the capture's own first legs. */}
            <StandaloneCapture
              study={value}
              opening={value.cases.length === 0 ? openingOfExclusions(value.exclusions) : NO_OPENING}
            />
          </section>
        )}

      <p className="muted">
        <Link to={href.triage({ lane })}>← queue</Link>
      </p>
    </div>
  )
}

/**
 * The cell a link cited, from a run whose bundle is no longer on disk: the run
 * report reads what the store still holds of it — its row, or the summary a
 * citation kept once the wipe dropped the row.
 */
const CitedRunGone = (
  { capture, runId, lane, caseId, cell }: {
    capture: string
    runId: number
    lane: string | undefined
    caseId: string | undefined
    cell: string
  }
): JSX.Element => (
  <p className="report-summary-note" title={`the cited cell (${cell}) went with its bundle`}>
    <strong>run {runId} not on disk</strong> ·{" "}
    <Link to={href.report({ run: `@run:${runId}`, lane })}>report</Link> ·{" "}
    <a
      href={`/api/triage/${encodeURIComponent(capture)}/runs/run${runId}/text${
        caseId === undefined ? "" : `?case=${encodeURIComponent(caseId)}`
      }`}
    >
      text
    </a>
  </p>
)

/**
 * The capture ladder alone, for a capture nothing has replayed yet or a refused
 * call. With an `opening` it is that call's legs alone, as a cell's capture pane
 * opens, and the reader widens it to the capture around them on purpose.
 */
const StandaloneCapture = (
  { study, opening }: { study: TriageStudy; opening: CaptureOpening }
): JSX.Element | null => {
  const [selected, setSelected] = useState<FlowsRow | undefined>(undefined)
  const [onlySelected, setOnlySelected] = useState(true)
  const filters = useCaptureFilters()
  const legs = useMemo(() => [...(opening.selection?.legs ?? NO_LEGS)], [opening.selection])
  const flows = useCaptureFlows(study.capture, study.flows, legs)
  const read = useMemo(
    () => (flows.document === undefined ? undefined : parseFlowsDocument(flows.document)),
    [flows.document]
  )
  const layout = useMemo(
    () =>
      read !== undefined && read.ok
        ? buildFlowsLayout(read.document, {
          selection: opening.selection,
          sut: opening.sut,
          onlySelected,
          legs: filters.legs.value,
          sockets: filters.sockets.value,
          title: study.capture
        })
        : undefined,
    [read, study, opening, onlySelected, filters.legs.value, filters.sockets.value]
  )
  if (study.flows === undefined) return <p className="muted">no flows document</p>
  if (flows.error !== undefined) return <p className="error">{study.flows.path}: {flows.error}</p>
  if (read !== undefined && !read.ok) return <p className="error">{study.flows.path}: {read.reason}</p>
  return (
    <div className="panel standalone-capture">
      <header className="study-pane-head">
        <h3>capture</h3>
        <CopyChip label="path" value={study.flows.path} />
      </header>
      {layout && (
        <FlowsPane
          layout={layout}
          selected={selected}
          onSelect={(row) => setSelected(row)}
          onlySelected={opening.selection === undefined
            ? undefined
            : { value: onlySelected, onChange: setOnlySelected }}
          legs={filters.legs}
          sockets={filters.sockets}
          tag={filters.tag}
          window={{ ...flows.window, beyond: flows.beyond, total: study.flows.legs, onWindow: flows.onWindow }}
        />
      )}
    </div>
  )
}

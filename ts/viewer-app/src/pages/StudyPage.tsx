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
  NO_HTTP_EXCHANGES,
  parseFlowsDocument,
  parseRecording,
  type FlowsRow,
  type LadderEvent
} from "@sip/viewer-core"
import type { JSX, ReactNode } from "react"
import { useEffect, useMemo, useState } from "react"
import { api, useAsync, type CaseRun, type TriageRefusal, type TriageStudy, type TriageStudyCell } from "../api.ts"
import { useCaptureFlows } from "../capture-flows.ts"
import { useHeld } from "../held.ts"
import { laneOf, rememberLane } from "../lane-memory.ts"
import { Age } from "../components/Age.tsx"
import { useCaptureFilters } from "../components/capture-filters.ts"
import { CallLanes } from "../components/CallLanes.tsx"
import { Confrontation } from "../components/Confrontation.tsx"
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
import { SipMessageView } from "../components/SipMessageView.tsx"
import { StudyHead } from "../components/StudyHead.tsx"
import { VerdictPane } from "../components/VerdictPane.tsx"
import { findingsByStep, marksOnLadder } from "../findings.ts"
import { useCommands } from "../commands.ts"
import { launchRef } from "../launch.ts"
import { cellDocument, cellOpening, NO_OPENING, openingOfExclusions, type CaptureOpening } from "../opening.ts"
import { href, setQuery, useQuery } from "../router.ts"
import { captureSteps, focusStep, recordingSteps, scenarioSteps, stepOfEvent, type Focus } from "../steps.ts"

/**
 * A cell passed when its verdict says so. `ok-negative` is a pass — a case that
 * passed BY failing exactly as declared — and nothing else is.
 */
const cellPassed = (cell: TriageStudyCell): boolean =>
  cell.status === "ok" || cell.status === "ok-negative"

/** A ladder about no leg in particular: the capture's own first legs are drawn. */
const NO_LEGS: readonly number[] = []

/** The CALL a cell replayed: a cell dir is `<case>__<shape>__<lane>`. */
const caseIdOf = (cellName: string): string => cellName.split("__")[0] ?? cellName

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
      {copied ? "copied" : `copy "${command} ${(caseId ?? capture).slice(0, 16)}…"`}
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
      <span className="muted">this call:</span>
      <span className={`tag ${cellPassed(cell) ? "matched" : "missing"}`}>
        {cell.status ?? "no verdict.json"}
      </span>
      <Age at={cell.writtenAt} prefix="replayed" />
      {cell.failedStep !== undefined && (
        <FailedSteps steps={[cell.failedStep]} onPick={onPickStep} />
      )}
      <span className="muted">deltas vs the capture:</span>
      {deltas === undefined
        ? <span className="muted">no classification</span>
        : <Deltas counts={deltas} detail />}
    </p>
  )
}

/** The evidence panes, and which of them are open. */
const PANES = [
  { key: "capture", label: "capture (pcap)" },
  { key: "case", label: "case (ladder + json)" },
  { key: "replay", label: "replay (recording)" },
  { key: "http", label: "http (exchanges)" }
] as const

type PaneKey = typeof PANES[number]["key"]

/** One evidence pane: its own hide control, so a wide capture can be put away. */
const Pane = (
  { title, path, wide, onHide, children }: {
    title: string
    path?: string | undefined
    /** A pane that reads as prose, not as a ladder: it takes the whole row. */
    wide?: boolean
    onHide: () => void
    children: ReactNode
  }
): JSX.Element => (
  <section className={wide === true ? "study-pane wide" : "study-pane"}>
    <header className="study-pane-head">
      <h3>{title}</h3>
      <button type="button" className="copy-ref" onClick={onHide}>hide</button>
    </header>
    {path !== undefined && <p className="muted path">{path}</p>}
    {children}
  </section>
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
      <span className="muted">this call alone:</span>
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

  // The URL's step is a selection like any other, applied once the layouts that
  // resolve it exist. It only ever ADDS a focus: a click on a message no step
  // cites must not be undone by the URL saying nothing.
  useEffect(() => {
    if (step !== undefined) setFocus(focusStep(step, indexes))
  }, [step, indexes])

  const pickStep = (next: string): void => {
    setFocus(focusStep(next, indexes))
    onStep(next)
  }
  const clearStep = (): void => {
    setFocus({})
    onStep(undefined)
  }
  // Clicking the message already selected UNSELECTS it, on every pane at once:
  // the detail panels are sticky, and three of them open is most of the
  // viewport spent on a message the reader has finished with.
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
  // The column count is the LADDER panes': the http pane takes a row of its own.
  const open = PANES.filter((pane) => shown.has(pane.key) && pane.key !== "http")

  return (
    <>
      <Outcome cell={cell} classification={detail?.classification} onPickStep={pickStep} />
      <OwnRun study={study} cell={cell} session={session} lane={lane} />
      {skipped === undefined
        ? (
          <>
            <VerdictPane verdict={detail?.verdict} onPickStep={pickStep} />
            <RfcAuditPane rfc={detail?.rfc} />
          </>
        )
        : (
          <p className="skipped-note">
            <strong>skipped</strong> — the document declares this lane{" "}
            <code>{skipped}</code>, and a driver skips a blocked lane rather than
            failing it. Nothing was asked of the SUT, so this cell has no verdict
            and no replay ladder; the capture and the case below are the evidence.
          </p>
        )}
      {/* A cell with no document of a call the standing cut refused: the
          record is the verdict, and the capture pane below is drawn from it. */}
      {!pivotRead.ok && study.exclusions !== undefined && opening.selection !== undefined && (
        <ExclusionPane exclusion={study.exclusions} caseId={caseIdOf(cell.name)} />
      )}
      {cell.refused && (
        <div className="refused-note">
          <h3>the interpreter refused this case — no bundle was produced</h3>
          <p className="muted">
            the run failed HERE and has no recording to show for it: the case below is the
            evidence, and the refusal is what to fix.
          </p>
          <pre className="raw">{detail?.error ?? "loading the refusal…"}</pre>
        </div>
      )}
      {detail?.stderr !== undefined && (
        <details className="study-record cell-stderr" open>
          <summary>stderr.txt — what the interpreter said</summary>
          <pre className="raw">{detail.stderr}</pre>
        </details>
      )}
      {evidence.state === "error" && (
        <p className="error">this cell's evidence did not load: {evidence.message}</p>
      )}
      {detail?.confrontationProblem !== undefined && (
        <p className="error">confrontation output did not decode: {detail.confrontationProblem}</p>
      )}

      <div className="pane-toolbar">
        <span className="muted">panes:</span>
        {PANES.map((pane) => (
          <label key={pane.key}>
            <input
              type="checkbox"
              checked={shown.has(pane.key)}
              onChange={() => toggle(pane.key)}
            />
            {pane.label}
          </label>
        ))}
        {focus.step === undefined
          ? <span className="muted">click a message to read it; click it again to put it away.</span>
          : (
            <span className="focus-note">
              step <code>{focus.step}</code> on every ladder that draws it
              <button type="button" className="copy-ref" onClick={clearStep}>clear</button>
            </span>
          )}
      </div>

      <div className="study-panes" data-open={open.length}>
        {shown.has("capture") && (
          <Pane
            title="capture — what the wire carried"
            path={study.flows?.path}
            onHide={() => toggle("capture")}
          >
            {study.flows === undefined && (
              <p className="muted">no flows document was found for this capture.</p>
            )}
            {flows.error !== undefined && (
              <p className="error">{study.flows?.path}: {flows.error}</p>
            )}
            {flowsRead !== undefined && !flowsRead.ok && (
              <p className="error">{study.flows?.path}: {flowsRead.reason}</p>
            )}
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
              />
            )}
          </Pane>
        )}
        {shown.has("case") && (
          <Pane
            title="case — the script cut from the selection"
            path={`${cell.dir}/pivot.json`}
            onHide={() => toggle("case")}
          >
            <p className="muted">
              the v3 document the run replayed, drawn: every step's <code>observed</code> is the
              capture coordinate it came from, so a step here and a datagram there are one message.
            </p>
            {/* The ladder leads; the detector roster is long enough to bury it. */}
            <details className="study-record">
              <summary>detectors and generator flags</summary>
              <Flags annotations={annotationsOf(document)} />
            </details>
            {evidence.state === "loading" && document === undefined
              ? <p className="muted">loading this cell's document…</p>
              : !pivotRead.ok
              ? <p className="error">case document: {pivotRead.reason}</p>
              : caseLayout && (
                <LadderPane
                  layout={caseLayout}
                  selectedId={focus.scenario}
                  onSelect={pickCase}
                  marksById={caseMarks}
                  legend={false}
                  detail={caseSelected === undefined
                    ? undefined
                    : <JsonView value={caseSelected.source} label={`step ${caseSelected.id}`} />}
                />
              )}
            <details className="study-record">
              <summary>pivot.json</summary>
              <JsonView value={document} label="pivot.json" />
            </details>
          </Pane>
        )}
        {shown.has("replay") && (
          <Pane
            title="replay — what the run produced"
            path={cell.dir}
            onHide={() => toggle("replay")}
          >
            {evidence.state === "loading"
              ? <p className="muted">loading this cell's recording…</p>
              : (
                <LadderPane
                  layout={runLayout}
                  selectedId={focus.replay}
                  onSelect={pickReplay}
                  marksById={replayMarks}
                  legend={false}
                  detail={replaySelected === undefined
                    ? undefined
                    : (
                      <SipMessageView
                        record={replaySelected.source}
                        label={`replay · ${replaySelected.id}`}
                      />
                    )}
                />
              )}
          </Pane>
        )}
        {shown.has("http") && (
          <Pane
            title="http — the exchange as replayed"
            path={`${cell.dir}/http.json`}
            wide
            onHide={() => toggle("http")}
          >
            <Exchange exchanges={served.exchanges} warnings={served.warnings} />
            {/* What the mock was SET to answer, as the cut wrote it: what the
                exchange above should be read against. */}
            <details className="study-record">
              <summary>what the mock was set to answer</summary>
              <Profile ledger={studyCase?.ledger} />
            </details>
          </Pane>
        )}
      </div>

      <section className="confrontation-section">
        <h3>confrontation — per-message delta</h3>
        <p className="muted">
          click a <code>step</code> to put that message on every ladder above; clicking a message on
          a ladder marks its rows here.
        </p>
        {evidence.state === "loading"
          ? <p className="muted">loading this cell's delta sheet…</p>
          : (detail?.confrontation ?? []).length === 0
          ? <p className="muted">no confrontation records: nothing differed, or nothing was compared.</p>
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
            <summary>call.terminated.json — the calls the SUT reported terminated</summary>
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
    <span className="muted">
      {calls.length} call(s) cut from this capture, each ruled on by itself:
    </span>
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
        {call.failedStep !== undefined && <span className="call-step">died at {call.failedStep}</span>}
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
    <h3>the cut refused this call — no case was made of it</h3>
    <p className="counts">
      {refusal.reasons.map((reason) => <span key={reason} className="tag error-refused">{reason}</span>)}
    </p>
    {refusal.callIds.length > 0 && (
      <p className="muted">
        calls:{" "}
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
  // The workdir holds every lane's bundles side by side, and the lanes carry
  // different defects — so a study opened on one lane reads that lane's cells
  // and nothing else. A cell whose name states no lane is shown either way.
  const cells = loaded === undefined
    ? []
    : loaded.sessions.flatMap((session) =>
      session.cells
        .filter((cell) => lane === undefined || cell.lane === undefined || cell.lane === lane)
        .map((cell) => ({ key: `${session.name}/${cell.name}`, session, cell }))
    )
  // The default cell is the LATEST run's first failing cell (else its first):
  // after a re-run the page must show what just happened, not the oldest
  // failure. An explicit pick — the reader's, or the URL's — still wins.
  const lastSession = cells[cells.length - 1]?.session
  const latest = cells.filter((entry) => entry.session === lastSession)
  const ran = latest.filter((entry) => entry.cell.skipped === undefined)
  const active = cells.find((entry) => entry.key === cellKey) ??
    (queryCase === undefined
      ? undefined
      : latest.find((entry) => caseIdOf(entry.cell.name) === queryCase)) ??
    ran.find((entry) => !cellPassed(entry.cell)) ?? ran[0] ?? latest[0] ?? cells[0]
  const activeCase = active === undefined ? undefined : caseIdOf(active.cell.name)
  // The header follows the CELL on screen: picking an older run must not leave
  // the page stating the latest run's number over an older run's ladders.
  const activeSession = active?.session
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
          <p className="muted">
            {study.loading && <span className="tag">reloading…</span>}
            {study.error !== undefined && <span className="tag missing">{study.error}</span>}{" "}
            {value.standing === undefined
              ? `never run on ${queryLane ?? "the configured lane"}`
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
                <span className={`tag ticket-${value.ticket.state}`}>{value.ticket.state}</span>
              </>
            )}
          </p>
          {/* What the deployment's hooks recorded around the run on screen —
              the box's own account of that replay, kept beside it. */}
          {(activeSession ?? value.sessions[value.sessions.length - 1])?.hooks.map((hook) => (
            <details key={hook.name} className="rerun-log study-hook">
              <summary className="muted">hook: {hook.name}</summary>
              <pre className="raw">{hook.text}</pre>
            </details>
          ))}
          {/* Where the cases and the flows doc below come from. The lanes pin
              their own preps, so the standing cut and the run that produced the
              ladders are stated apart rather than as one fact. */}
          {value.cut !== undefined && (
            <p className="muted path">
              cut: prep{value.cut.prepId} · {value.cut.cases} case(s) ·{" "}
              <Age at={value.cut.cutAt} />
              {value.cut.broke !== null && <span className="error"> · broke: {value.cut.broke}</span>}
            </p>
          )}
          {value.corpusDir !== undefined && <p className="muted path">corpus: {value.corpusDir}</p>}
          {value.sessionDir !== undefined && <p className="muted path">session: {value.sessionDir}</p>}
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
          {cells.length > 1 && (
            <select
              value={active?.key ?? ""}
              onChange={(event) => setCellKey(event.target.value)}
            >
              {cells.map((entry) => (
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
          <h3>the current cut refuses this capture</h3>
          <p className="muted">
            run{value.standing.runId} produced no case, so there is nothing to replay and nothing
            to triage here — the work is the generator's. Any ladder below comes from an earlier
            cut that did produce one.
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
                  no replay bundle yet
                  {commands.triage !== undefined && (
                    <> — launch <code>{launchRef(commands.triage, value.capture, lane, value.case)}</code> to produce one</>
                  )}. The cut cases below are what a session would replay.
                </p>
              )
              : <ExclusionPane exclusion={value.exclusions} />}
            {value.cases.length === 0
              ? <p className="muted">no cases were cut from this capture.</p>
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
        <Link to={href.triage({ lane })}>back to the queue</Link>
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
  <p className="report-summary-note">
    <strong>run {runId} is not on disk</strong> — the cell this link cites (<code>{cell}</code>) went with its
    bundle. <Link to={href.report({ run: `@run:${runId}`, lane })}>run {runId}'s report</Link> reads what the
    store still holds of it, the summary a citation kept included;{" "}
    <a
      href={`/api/triage/${encodeURIComponent(capture)}/runs/run${runId}/text${
        caseId === undefined ? "" : `?case=${encodeURIComponent(caseId)}`
      }`}
    >
      its text
    </a>{" "}
    says the same. The page below shows the runs on disk.
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
  if (study.flows === undefined) return <p className="muted">no flows document was found for this capture.</p>
  if (flows.error !== undefined) return <p className="error">{study.flows.path}: {flows.error}</p>
  if (read !== undefined && !read.ok) return <p className="error">{study.flows.path}: {read.reason}</p>
  return (
    <div>
      <p className="muted path">{study.flows.path}</p>
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

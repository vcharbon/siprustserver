/**
 * One run as one plain text: its identity and per-cell verdicts, the capture
 * ladder, a replay ladder per cell drawn (`drawnCells`), and each drawn cell's
 * SUT logs (its `sut-log` and `lane-log` attachments), in that order.
 *
 * It is the study page flattened, not a second reading of it: the same study
 * payload, the same layouts, rendered through `flowsToText`/`ladderToText`, so
 * a run can be pasted into a ticket, diffed against another, or read by an
 * agent that has no browser. It reads what the run kept and asks the lab
 * nothing: a log asked again is an on-demand collector's (`collect.ts`).
 */
import {
  exclusionSelection,
  exclusionSut,
  parseExclusions,
  type ExclusionRecord
} from "@sip/viewer-core/exclusion"
import {
  buildFlowsLayout,
  captureSelection,
  chunkedFlows,
  documentSut,
  ladderWindow,
  legWindows,
  parseFlowsDocument,
  type CaptureSelection,
  type FlowsChunk
} from "@sip/viewer-core/flows"
import type { PivotV3 } from "@sip/viewer-core/pivot"
import { buildRecordingLayout, parseRecording } from "@sip/viewer-core/recording"
import { exclusionsToText, flowsToText, fullLadderToText, ladderToText } from "@sip/viewer-core/text"
import { pivotOf } from "@sip/corpus/report/call-ladders"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { NotFound } from "./catalog.ts"
import { ViewerConfig } from "./config.ts"
import { callLog, callLogText, identitiesOf, refIdsOf } from "./call-log.ts"
import { sutLogRole } from "./lane-roles.ts"
import { partyLines, relayedLines, verdictWord } from "./rfc-text.ts"
import { RunTrace } from "./run-trace.ts"
import { areSegments } from "./segment.ts"
import { RunWindow } from "./run-window.ts"
import { SitePack } from "./site-pack.ts"
import { TriageCatalog } from "./triage.ts"

export interface Interface {
  /**
   * The whole of one run as text. `run` is the run directory NAME the study
   * listed, never a path, and a run this capture has none of is a 404. An
   * export of a run whole past `WHOLE_RUN_LADDERS` cells lists every cell's
   * verdict, and draws the evidence, ladder and SUT logs of `drawnCells`
   * alone. `full` unfolds every
   * payload, SIP bodies and HTTP exchanges inline, and, for a
   * run of one call, names the traces that call left behind. `caseId` reads
   * the run as the study of that ONE call reads it: its cells alone, so a run
   * over a capture of thousands of calls exports one replay ladder; a call the
   * capture holds nothing of is a 404 naming both. A call the lane SKIPPED
   * has a cell with no bundle: the capture ladder draws the legs its cut
   * document cites, as a bundle's own pivot would. A call the cut REFUSED has
   * no cell: its exclusion record stands where the verdict would, and the
   * capture ladder draws the legs that record names, oriented on the SUT the
   * cut stated. `cell` reads the run as that ONE cell of it, a cell the run
   * holds none of being a 404. An export of one call (`case` or `cell`) reads
   * that call's SUT logs, cut to its own window, for the call alone
   * (`call-log.ts`) unless `full`. No export names a local path.
   */
  readonly text: (
    capture: string,
    run: string,
    lane: string | undefined,
    full?: boolean,
    caseId?: string,
    cell?: string
  ) => Effect.Effect<string, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/RunText") {}

/** Where the legs a capture ladder is about come from. */
type Focus = "cell" | "exclusion" | "window"

/** The window of legs the export draws, and what it says of the rest. */
const drawnLegs = (
  window: { readonly from: number; readonly count: number },
  total: number,
  focus: Focus
): string =>
  focus === "cell"
    ? `the call's own legs of ${total} drawn; the run whole draws the capture around them\n`
    : focus === "exclusion"
    ? `the refused call's legs of ${total} drawn, as its exclusion record names them\n`
    : window.count === total
    ? ""
    : `legs ${window.from}-${window.from + window.count - 1} of ${total} drawn\n`

/** The run directory name the driver writes, and the run id inside it. */
const RUN_DIR = /^run(\d+)$/

/** One section of the export: a `== <title>` line, then its body. */
const section = (title: string, body: string): string => `== ${title}\n${body.replace(/\s+$/, "")}\n`

/**
 * A cell passed when the run's index says so — the verdict the store records,
 * the RFC audit and the bless included. A run that wrote no index answers by
 * its interpreter's word: `ok-negative` is a pass — a case that passed BY
 * failing exactly as declared — and nothing else is.
 */
const passed = (cell: TriageCatalog.StudyCell): boolean =>
  cell.passed ?? (cell.status === "ok" || cell.status === "ok-negative")

/** The replay ladders an export of a run whole draws at most. */
export const WHOLE_RUN_LADDERS = 20

/**
 * The cells whose evidence and ladder an export draws, in run order: every cell
 * of a one-call export; of a run whole, `limit` cells at most, a failed cell
 * taken ahead of a passed one.
 */
export const drawnCells = (
  cells: readonly TriageCatalog.StudyCell[],
  oneCall: boolean,
  limit = WHOLE_RUN_LADDERS
): readonly TriageCatalog.StudyCell[] => {
  if (oneCall || cells.length <= limit) return cells
  const taken = new Set([...cells.filter((cell) => !passed(cell)), ...cells.filter(passed)].slice(0, limit))
  return cells.filter((cell) => taken.has(cell))
}

/** The classification counts as one line; absent where the cell wrote none. */
const deltas = (value: unknown): string | undefined => {
  if (typeof value !== "object" || value === null) return undefined
  const record = value as Record<string, unknown>
  const count = (field: string): number => typeof record[field] === "number" ? record[field] : 0
  return `${count("unknown")} unknown · ${count("unlisted")} unlisted · ` +
    `${count("known_bug")} known-bug · ${count("accepted")} accepted`
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const triage = yield* TriageCatalog.Service
    const traces = yield* RunTrace.Service
    const windows = yield* RunWindow.Service
    const runForm = (yield* SitePack.Service).triage.forms.run

    /** The cells of one run, each with the evidence its ladder is drawn from. */
    const evidence = Effect.fn("RunText.evidence")(function*(
      capture: string,
      run: string,
      cells: readonly TriageCatalog.StudyCell[]
    ) {
      return yield* Effect.forEach(cells, (cell) =>
        triage.cell(capture, run, cell.name).pipe(
          Effect.match({
            onFailure: () => ({ cell, detail: undefined }),
            onSuccess: (detail) => ({ cell, detail })
          })
        ))
    })

    /** What the capture ladder is about: the legs it opens on, and what orients it. */
    interface Opening {
      readonly selection: CaptureSelection | undefined
      readonly sut: readonly string[]
      readonly focus: Focus
    }

    /**
     * The capture ladder as the study's capture pane draws it, read the way
     * the page reads it — the envelope once, then windows of legs — so a
     * capture no response carries whole still draws here. Over one call it is
     * that call's legs alone, read exactly, however far apart they lie; over a
     * refused call, the legs its record names, the same; over the run whole,
     * the window around the first cell's legs.
     */
    const captureLadder = Effect.fn("RunText.captureLadder")(function*(
      capture: string,
      opening: Opening,
      flows: { readonly path: string; readonly legs: number } | undefined
    ) {
      if (flows === undefined) return "no flows document for this capture"
      const envelope = yield* triage.flowsEnvelope(capture).pipe(
        Effect.orElseSucceed(() => null)
      )
      if (envelope === null) return "no flows document for this capture"
      const { selection, sut } = opening
      const focusLegs = [...(selection?.legs ?? [])]
      const window = ladderWindow(envelope.legs, focusLegs)
      const windows = opening.focus === "window" ? [window] : legWindows(envelope.legs, focusLegs)
      const chunks = yield* Effect.forEach(windows, (asked): Effect.Effect<FlowsChunk> =>
        triage.flowsLegs(capture, asked.from, asked.count).pipe(
          Effect.orElseSucceed((): FlowsChunk => ({ from: asked.from, legs: [] }))
        ))
      const read = parseFlowsDocument(chunkedFlows(envelope.document, envelope.legs, chunks))
      if (!read.ok) return `the flows document does not read: ${read.reason}`
      return drawnLegs(window, envelope.legs, opening.focus) +
        flowsToText(buildFlowsLayout(read.document, {
          selection,
          sut,
          onlySelected: opening.focus !== "window",
          title: capture
        }))
    })

    /** The generator's records the study carries, where it carries any that read. */
    const exclusionsOf = (
      exclusions: { readonly document: unknown } | undefined
    ): readonly ExclusionRecord[] => {
      if (exclusions === undefined) return []
      const read = parseExclusions(exclusions.document)
      return read.ok ? read.records : []
    }

    /**
     * The document a cell is drawn from: its bundle's pivot, else — a cell the
     * lane skipped holds no bundle — the standing cut's document of its case.
     */
    const documentOf = (
      cell: TriageCatalog.StudyCell,
      detail: TriageCatalog.StudyCellDetail | undefined,
      cases: readonly TriageCatalog.StudyCase[]
    ): PivotV3 | undefined =>
      pivotOf(detail?.pivot) ??
        pivotOf(cases.find((entry) => cell.name.startsWith(`${entry.id}__`))?.document)

    /**
     * What the ladder opens on, in order of precedence: the first cell's
     * document — its bundle's pivot, else its case's cut document — which
     * cites the capture through its steps; else the exclusion records the
     * study carries, which name the refused call's legs and the cut's SUT;
     * else the capture's own first window.
     */
    const openingOf = (
      cells: readonly {
        readonly cell: TriageCatalog.StudyCell
        readonly detail: TriageCatalog.StudyCellDetail | undefined
      }[],
      cases: readonly TriageCatalog.StudyCase[],
      exclusions: readonly ExclusionRecord[],
      oneCall: boolean
    ): Opening => {
      const selected = cells.map(({ cell, detail }) => documentOf(cell, detail, cases)).find((document) =>
        document !== undefined
      )
      if (selected !== undefined) {
        return {
          selection: captureSelection(selected),
          sut: documentSut(selected),
          focus: oneCall ? "cell" : "window"
        }
      }
      const selection = exclusionSelection(exclusions)
      return selection.empty
        ? { selection: undefined, sut: [], focus: "window" }
        : { selection, sut: exclusionSut(exclusions), focus: "exclusion" }
    }

    /**
     * How one cell ended: the verdict and what the deltas came to in a line,
     * then a line per RFC finding the document attributes to another party and
     * per binding finding charged to a scripted party. A cell whose evidence
     * the export did not draw (`drawn` false) states its verdict alone and says so.
     */
    const outcome = (
      cell: TriageCatalog.StudyCell,
      detail: TriageCatalog.StudyCellDetail | undefined,
      drawn = true
    ): string => {
      if (cell.skipped !== undefined) return `${cell.name}: skipped — ${cell.skipped}`
      if (cell.refused) {
        return drawn
          ? `${cell.name}: refused — ${detail?.error ?? "no bundle"}`
          : `${cell.name}: refused (evidence not drawn)`
      }
      const verdict = `${verdictWord(passed(cell), cell.sourceFault === true)} · ${cell.status ?? "no verdict"}` +
        `${cell.failedStep === undefined ? "" : ` at ${cell.failedStep}`}`
      if (!drawn) return `${cell.name}: ${verdict} (evidence not drawn)`
      const classification = deltas(detail?.classification)
      return [
        `${cell.name}: ${verdict}${classification === undefined ? "" : ` · ${classification}`}`,
        ...relayedLines(detail?.rfc),
        ...partyLines(detail?.rfc)
      ].join("\n")
    }

    /**
     * The traces of a ONE-CALL run, named with the route that serves each. A
     * run of many calls names none: a text about hundreds of cells would be a
     * file listing, and the study page addresses a cell's traces per cell.
     */
    const traceSection = Effect.fn("RunText.traceSection")(function*(
      capture: string,
      run: string,
      cells: readonly TriageCatalog.StudyCell[]
    ) {
      const only = cells.length === 1 ? cells[0] : undefined
      if (only === undefined) return []
      const found = yield* traces.list(capture, run, only.name).pipe(
        Effect.orElseSucceed(() => [] as readonly RunTrace.Trace[])
      )
      if (found.length === 0) return []
      const route = `/api/triage/${encodeURIComponent(capture)}/runs/${encodeURIComponent(run)}` +
        `/cells/${encodeURIComponent(only.name)}/trace`
      return [section(
        "traces",
        [
          `${only.name}: ${found.length} recorded`,
          ...found.map((trace) =>
            `  ${trace.kind} ${trace.file} (${trace.bytes}B)` +
            `\n    GET ${route}?file=${encodeURIComponent(trace.file)}`
          )
        ].join("\n")
      )]
    })

    const text = Effect.fn("RunText.text")(function*(
      capture: string,
      run: string,
      lane: string | undefined,
      full = false,
      caseId?: string,
      only?: string
    ) {
      // The capture and the run are NAMES the study listed, never paths
      // (`./segment.ts`): either one joined unchecked reads outside this
      // capture's workdir.
      if (!areSegments(capture, run)) return yield* new NotFound({ id: `${capture}/${run}`, accepted: runForm })
      const study = yield* triage.study(capture, lane, caseId)
      const session = study.sessions.find((entry) => entry.name === run)
      if (session === undefined) return yield* new NotFound({ id: `${capture}/${run}`, accepted: runForm })

      const digits = RUN_DIR.exec(run)?.[1]
      const window = yield* windows.of(digits === undefined ? undefined : Number(digits), session.dir)
      const oneCall = caseId !== undefined || only !== undefined
      const kept = only === undefined ? session.cells : session.cells.filter((cell) => cell.name === only)
      if (only !== undefined && kept.length === 0) {
        return yield* new NotFound({ id: `${capture}/${run}/${only}`, note: "the run holds no such cell" })
      }
      const drawn = drawnCells(kept, oneCall)
      const capped = drawn.length < kept.length
      const cells = yield* evidence(capture, run, drawn)
      const details = new Map(cells.map(({ cell, detail }) => [cell.name, detail]))
      const verdicts = kept.map((cell) => outcome(cell, details.get(cell.name), details.has(cell.name)))
      // What the cut refused, where it is THIS export's: over one call the
      // records are that call's own, and a run with no cell at all has only
      // them to state. Beside a run's many cells they are other calls', and
      // neither the verdicts nor the ladder read them.
      const refused = caseId !== undefined || kept.length === 0 ? exclusionsOf(study.exclusions) : []
      const laneRead = lane ?? config.triage?.lane

      const identity = [
        `capture: ${capture}`,
        ...(caseId === undefined ? [] : [`case: ${caseId}`]),
        `lane: ${laneRead ?? "-"}`,
        `run: ${run}${digits === undefined ? "" : ` (id ${digits})`}`,
        ...(session.forced === undefined ? [] : [`forced: the lane lacks ${session.forced.join(", ")}`]),
        `started: ${window.startedAt}`,
        `finished: ${window.finishedAt}`,
        `window: ${window.source}`,
        `cells: ${kept.length}`,
        ...(capped
          ? [`drawn: ${drawn.length} of ${kept.length} cells, the failed ones first; add &case=<case id> to read one call`]
          : []),
        ...verdicts,
        ...(refused.length > 0 ? [exclusionsToText(refused)] : [])
      ].join("\n")

      // The capture ladder is drawn as the study's capture pane draws it: a
      // window of legs opening on the ones the first cell's selection marks,
      // which are the legs the case was cut from. A capture of thousands of
      // calls states the window it drew rather than drawing them all.
      const capturePane = yield* captureLadder(
        capture,
        openingOf(cells, study.cases, refused, caseId !== undefined),
        study.flows
      )

      const replays = cells.map(({ cell, detail }) => {
        const recording = parseRecording(detail?.recording ?? {})
        const exchanges = detail?.http?.exchanges ?? []
        const layout = buildRecordingLayout(recording, {
          document: pivotOf(detail?.pivot),
          title: cell.name,
          subtitle: `${recording.legs.length} legs · ${recording.messages.length} messages` +
            `${full ? ` · ${exchanges.length} http exchanges` : ""}`
        })
        return section(
          `replay ${cell.name}`,
          full ? fullLadderToText(layout, exchanges) : ladderToText(layout)
        )
      })

      const call = oneCall && cells.length === 1 ? cells[0]?.detail : undefined
      const identities = call === undefined
        ? []
        : identitiesOf(call.join?.callIds ?? [], refIdsOf(call.http?.exchanges ?? []))
      const read = (text: string): string =>
        full || identities.length === 0 ? text : callLogText(callLog(text, identities), identities).join("\n")
      const titled = (kind: string, name: string, ...notes: ReadonlyArray<string | undefined>): string => {
        const said = notes.filter((note): note is string => note !== undefined)
        return `${kind} ${name}${said.length === 0 ? "" : ` (${said.join("; ")})`}`
      }
      const sutLogs = cells.flatMap(({ cell, detail }) =>
        (detail?.sutLogs ?? []).map((one) =>
          section(
            titled(one.name, cell.name, sutLogRole(one.title), "the call's window", `${one.bytes}B`),
            oneCall ? read(one.text) : one.text
          )
        )
      )

      return [
        section("run", identity),
        section("capture ladder", capturePane),
        ...replays,
        ...(full ? yield* traceSection(capture, run, kept) : []),
        ...sutLogs
      ].join("\n")
    })

    return Service.of({ text })
  })
)

export * as RunText from "./run-text.ts"

/**
 * The overview of ONE call's study (`case`): each lane's latest replay of it —
 * the page's lane first — with its result, its failing findings, its SUT
 * records and the links to its digest, messages, text and evidence; then the
 * call's cut document, its history across runs, and the legs of the capture
 * it was cut from. Everything it links is that call's: no capture-wide
 * resource, whatever the capture's size (`links-capture.ts` routes to calls).
 */
import { IdForms } from "@sip/corpus"
import { pivotOf } from "@sip/corpus/report/call-ladders"
import { CallHistory } from "@sip/store"
import { absolute, type Overview, type OverviewSection } from "@sip/viewer-core/routes"
import { captureSelection, type FlowsWindow, legWindows } from "@sip/viewer-core/flows"
import { Effect } from "effect"
import { trimLegs } from "./flows-trim.ts"
import { lanes } from "./lanes.ts"
import { sutOf } from "./lane-roles.ts"
import { cellPart, sized } from "./links-cell.ts"
import { SitePack } from "./site-pack.ts"
import { TriageCatalog } from "./triage.ts"

/** The runs of a call's history a study overview lists. */
const HISTORY = 10

const segment = encodeURIComponent

const text = (value: unknown): string => typeof value === "string" ? value : JSON.stringify(value)

const latestFirst = (entries: ReadonlyArray<CallHistory.Entry>, lane: string | undefined) =>
  [...entries].sort((a, b) => Number(b.lane === lane) - Number(a.lane === lane))

/** The page overview of the call `caseId` of `capture`'s study, at `query`. */
export const studyOverview = Effect.fn("Links.studyOverview")(function*(
  base: Overview,
  capture: string,
  asked: string,
  query: URLSearchParams,
  origin: string
) {
  const triage = yield* TriageCatalog.Service
  const forms = yield* IdForms.caseFormsOf
  const caseId = forms.caseIdIn(capture, asked)
  const lane = query.get("lane") ?? undefined
  const study = yield* triage.study(capture, lane, caseId).pipe(Effect.option)
  if (study._tag === "None") {
    return { ...base, title: `Study of call ${caseId}`, status: [`no study of ${caseId} in ${capture}`], sections: [] } satisfies Overview
  }
  const head = [
    `call ${caseId} of capture ${capture} · disposition ${study.value.disposition}` +
    (study.value.cut === undefined
      ? " · no standing cut"
      : ` · cut prep ${study.value.cut.prepId} at ${study.value.cut.cutAt}` +
        (study.value.cut.broke === null ? "" : `, broke: ${study.value.cut.broke}`))
  ]
  const captureSection = yield* captureLinks(study.value, capture, caseId, origin)
  const history = yield* CallHistory.Service
  const pack = yield* SitePack.Service
  const runs = yield* history.ofCall(capture, caseId, lanes(pack.lanes).map((one) => one.lane), HISTORY).pipe(
    Effect.orElseSucceed((): CallHistory.History => ({ latest: [], recent: [] }))
  )
  const laneSections = yield* Effect.forEach(
    latestFirst(runs.latest, lane),
    (entry) => laneSection(study.value, capture, caseId, entry, entry.lane === lane, query.get("step"), origin)
  )
  const word = (entry: CallHistory.Entry): string =>
    `${entry.lane} ${entry.skipped !== null ? `skipped (${entry.skipped})` : entry.passed ? "passed" : "failed"}` +
    `${entry.errorRule === null ? "" : ` ${entry.errorRule}`} (run ${entry.runId})`
  return {
    ...base,
    title: `Study of call ${caseId}`,
    status: [
      ...head,
      ...(study.value.refusal === undefined ? [] : [`refused by the cut: ${text(study.value.refusal)}`]),
      runs.latest.length === 0 ? "no lane ran this call" : latestFirst(runs.latest, lane).map(word).join(" · ")
    ],
    sections: [
      ...(yield* onScreenSection(capture, caseId, query.get("cell"), query.get("step"), runs, origin)),
      ...laneSections,
      callSection(study.value, capture, caseId, runs, origin),
      captureSection,
      pageData(study.value, runs, capture, caseId, lane, origin)
    ]
  } satisfies Overview
})

/** One lane's latest replay of the call: its result and findings, and the links to its evidence. */
const laneSection = Effect.fn("Links.studyLane")(function*(
  study: TriageCatalog.TriageStudy,
  capture: string,
  caseId: string,
  entry: CallHistory.Entry,
  onPage: boolean,
  step: string | null,
  origin: string
) {
  const run = `run${entry.runId}`
  const session = study.sessions.find((s) => s.name === run)
  const cell = session?.cells.find((c) => c.lane === entry.lane && c.name.startsWith(`${caseId}__`))
  const heading = `${entry.lane}: ${entry.skipped !== null ? "skipped" : entry.passed ? "passed" : "failed"}` +
    ` · run ${entry.runId}${onPage ? " (the page's lane)" : ""}`
  const lines = [
    `run ${entry.runId} · ${entry.setKind} set${entry.jobId === null ? "" : ` · job ${entry.jobId}`}` +
    ` · finished ${entry.finishedAt ?? "—"}`,
    ...(entry.skipped === null ? [] : [`skipped: ${entry.skipped}`]),
    ...(entry.errorRule === null ? [] : [`main error: ${entry.errorRule}`]),
    ...(entry.errorExpected === null ? [] : [`waited for: ${entry.errorExpected}`]),
    ...(entry.failedStep === null ? [] : [`failed at: ${entry.failedStep}`])
  ]
  if (cell === undefined) {
    return {
      heading,
      lines: [...lines, `SUT on this lane: ${sutOf(yield* SitePack.Service, entry.lane)}`, "no bundle of this run on disk"],
      links: []
    } satisfies OverviewSection
  }
  const part = yield* cellPart(capture, run, cell.name, entry.lane, { case: caseId }, origin, true, step ?? undefined)
  return { heading, lines: [...lines, ...part.lines], links: part.links } satisfies OverviewSection
})

/**
 * The replay the page has on screen (`cell=<run>/<cell>`), where it is not
 * a lane's latest one: its findings, its text and its evidence.
 */
const onScreenSection = Effect.fn("Links.studyOnScreen")(function*(
  capture: string,
  caseId: string,
  asked: string | null,
  step: string | null,
  runs: CallHistory.History,
  origin: string
) {
  const at = asked?.indexOf("/") ?? -1
  if (asked === null || at < 0) return []
  const run = asked.slice(0, at)
  const cell = asked.slice(at + 1)
  if (runs.latest.some((entry) => `run${entry.runId}` === run)) return []
  const lane = cell.split("__").at(-1) ?? ""
  const part = yield* cellPart(capture, run, cell, lane, { case: caseId }, origin, true, step ?? undefined)
  return [{
    heading: `On screen: ${run}/${cell}`,
    lines: part.lines,
    links: part.links
  } satisfies OverviewSection]
})

/** The call itself: its cut document and its runs across every lane. */
const callSection = (
  study: TriageCatalog.TriageStudy,
  capture: string,
  caseId: string,
  runs: CallHistory.History,
  origin: string
): OverviewSection => ({
  heading: "Call",
  lines: runs.recent.length === 0 ? [] : [
    "recent runs, newest first:",
    ...runs.recent.map((entry) =>
      `- run ${entry.runId} · ${entry.lane} · ${entry.setKind} · ${entry.passed ? "passed" : "failed"}` +
      `${entry.errorRule === null ? "" : ` ${entry.errorRule}`} · ${entry.finishedAt ?? "—"}`
    )
  ],
  links: [
    {
      label: sized(
        "case cut: the call's pivot document (JSON); one step with its captured message: `message?step=<id>` of a cell",
        JSON.stringify(study.cases.find((entry) => entry.id === caseId)?.document ?? null).length
      ),
      href: absolute(origin, `/api/triage/${segment(capture)}/document?case=${segment(caseId)}`)
    }
  ]
})

/** The windows of the capture's flows legs the call's cut document cites; none where it cites none. */
const callLegs = (study: TriageCatalog.TriageStudy, caseId: string | undefined): ReadonlyArray<FlowsWindow> => {
  const document = pivotOf(study.cases.find((entry) => entry.id === caseId)?.document)
  return document === undefined || study.flows === undefined
    ? []
    : legWindows(study.flows.legs, [...captureSelection(document).legs])
}

/** The legs of the capture the call was cut from: whole, and one line a message, sized. */
const captureLinks = Effect.fn("Links.studyCapture")(function*(
  study: TriageCatalog.TriageStudy,
  capture: string,
  caseId: string,
  origin: string
) {
  const triage = yield* TriageCatalog.Service
  const legsPath = (from: number, count: number) => `/api/triage/${segment(capture)}/flows/legs?from=${from}&count=${count}`
  const windows = callLegs(study, caseId)
  const callLinks = yield* Effect.forEach(windows, (window) =>
    triage.flowsLegs(capture, window.from, window.count).pipe(
      Effect.map((read) => {
        const legs = `${window.from}–${window.from + window.count - 1}`
        return [
          {
            label: sized(`the call's capture legs ${legs} (JSON, every message with its body)`, JSON.stringify(read).length),
            href: absolute(origin, legsPath(window.from, window.count))
          },
          {
            label: sized(`the same legs, each message in one line: time, addresses, start line`, JSON.stringify(trimLegs(read, "line")).length),
            href: absolute(origin, `${legsPath(window.from, window.count)}&raw=line`)
          }
        ]
      }),
      Effect.orElseSucceed(() => [])
    ))
  return {
    heading: "The call's capture legs",
    lines: windows.length === 0 ? ["the call's cut cites no leg of the capture"] : [],
    links: callLinks.flat()
  } satisfies OverviewSection
})

/** The page's own JSON, the call's alone, sized: its study and its history. */
const pageData = (
  study: TriageCatalog.TriageStudy,
  history: CallHistory.History,
  capture: string,
  caseId: string,
  lane: string | undefined,
  origin: string
): OverviewSection => ({
  heading: "Page data (JSON)",
  lines: [],
  links: [
    {
      label: sized("the call's study", JSON.stringify(study).length),
      href: absolute(
        origin,
        `/api/triage/${segment(capture)}?case=${segment(caseId)}${lane === undefined ? "" : `&lane=${segment(lane)}`}`
      )
    },
    {
      label: sized("the call's history across runs", JSON.stringify(history).length),
      href: absolute(origin, `/api/triage/${segment(capture)}/history?case=${segment(caseId)}`)
    }
  ]
})

/**
 * The request builder's pure half (§5 R1, R6, R9): what the filter bar, the
 * ticked rows and the population predicates come to as ONE selection the
 * server takes; the knobs as typed, held to numbers; and what a lane's
 * ceilings say about them. The page holds the state, this names its meaning.
 */
import type {
  LaneView,
  RequestKnobs,
  RequestLanePreview,
  RequestPreviewCapture,
  RequestSelection,
  TriageCallRow
} from "./api.ts"
import type { ChipFilter } from "./queue-filter.ts"

/** Which of the four forms the page is building. */
export type Mode = "filter" | "ticked" | "captures" | "population"

/** The filter bar as the page holds it: its words, and the lane the queue was read on. */
export interface Bar {
  /** The lane the page reads the queue on; "" where the page has none yet. */
  readonly lane: string
  readonly scope: string
  readonly q: string
  readonly filter: ChipFilter
}

/** The population predicates, as typed; "" = unstated. */
export interface Predicates {
  readonly universe: "all" | "standing"
  readonly maxSpan: string
  readonly greenOn: string
  readonly sample: string
  readonly limit: string
}

export const NO_PREDICATES: Predicates = { universe: "standing", maxSpan: "", greenOn: "", sample: "", limit: "" }

/** The key a ticked row is held under: the row's identity in the queue. */
export const rowKey = (row: Pick<TriageCallRow, "capture" | "caseId">): string => `${row.capture}#${row.caseId}`

const number = (text: string): number | undefined => {
  const value = Number(text)
  return text.trim() === "" || !Number.isFinite(value) ? undefined : value
}

const blank = (value: string): string | undefined => (value === "" ? undefined : value)

const some = (values: ReadonlyArray<string>): ReadonlyArray<string> | undefined =>
  values.length === 0 ? undefined : values

/**
 * The selection the page means, or why there is none: the filter as it
 * stands on the page's lane (whatever lanes the request targets, §5 R1, R2),
 * the ticked rows as cells (a case by its id, a refused call by its
 * key), the ticked rows' captures whole — every row on screen's where none
 * is ticked — or the population under its predicates.
 */
export const selectionOf = (
  mode: Mode,
  bar: Bar,
  ticked: ReadonlyArray<TriageCallRow>,
  shown: ReadonlyArray<TriageCallRow>,
  predicates: Predicates
): RequestSelection | string => {
  switch (mode) {
    case "filter":
      return {
        kind: "filter",
        ...(blank(bar.lane) === undefined ? {} : { lane: bar.lane }),
        scope: bar.scope,
        ...(blank(bar.q) === undefined ? {} : { q: bar.q }),
        ...(blank(bar.filter.include.rule) === undefined ? {} : { rule: bar.filter.include.rule }),
        ...(blank(bar.filter.include.expected) === undefined ? {} : { expected: bar.filter.include.expected }),
        ...(blank(bar.filter.include.refusal) === undefined ? {} : { refusal: bar.filter.include.refusal }),
        ...(blank(bar.filter.include.outcome) === undefined ? {} : { outcome: bar.filter.include.outcome }),
        ...(blank(bar.filter.include.feature) === undefined ? {} : { feature: bar.filter.include.feature }),
        ...(blank(bar.filter.include.source) === undefined ? {} : { source: bar.filter.include.source }),
        ...(some(bar.filter.exclude.rule) === undefined ? {} : { "not-rule": bar.filter.exclude.rule }),
        ...(some(bar.filter.exclude.expected) === undefined ? {} : { "not-expected": bar.filter.exclude.expected }),
        ...(some(bar.filter.exclude.refusal) === undefined ? {} : { "not-refusal": bar.filter.exclude.refusal }),
        ...(some(bar.filter.exclude.outcome) === undefined ? {} : { "not-outcome": bar.filter.exclude.outcome }),
        ...(some(bar.filter.exclude.feature) === undefined ? {} : { "not-feature": bar.filter.exclude.feature }),
        ...(some(bar.filter.exclude.source) === undefined ? {} : { "not-source": bar.filter.exclude.source })
      }
    case "ticked":
      if (ticked.length === 0) return "tick a row first"
      return {
        kind: "cells",
        cells: ticked.map((row) =>
          row.callKey !== null
            ? { capture: row.capture, callKey: row.callKey }
            : { capture: row.capture, caseId: row.caseId }
        )
      }
    case "captures": {
      const captures = [...new Set((ticked.length === 0 ? shown : ticked).map((row) => row.capture))].sort()
      return captures.length === 0 ? "no capture on screen" : { kind: "captures", captures }
    }
    case "population":
      return {
        kind: "universe",
        universe: predicates.universe,
        ...(number(predicates.maxSpan) === undefined ? {} : { maxSpan: number(predicates.maxSpan) }),
        ...(blank(predicates.greenOn) === undefined ? {} : { greenOn: predicates.greenOn }),
        ...(number(predicates.sample) === undefined ? {} : { sample: number(predicates.sample) }),
        ...(number(predicates.limit) === undefined ? {} : { limit: number(predicates.limit) })
      }
  }
}

/** The selection as the request command's line would state it, after `--lane`. */
export const describeSelection = (selection: RequestSelection): string => {
  switch (selection.kind) {
    case "filter": {
      const words = [
        ...(selection.lane === undefined ? [] : [`--on-lane ${selection.lane}`]),
        `--scope ${selection.scope ?? "failed"}`,
        ...(selection.q === undefined ? [] : [`--q ${selection.q}`]),
        ...(selection.rule === undefined ? [] : [`--rule ${selection.rule}`]),
        ...(selection.expected === undefined ? [] : [`--expected ${selection.expected}`]),
        ...(selection.refusal === undefined ? [] : [`--refusal ${selection.refusal}`]),
        ...(selection.outcome === undefined ? [] : [`--outcome ${selection.outcome}`]),
        ...(selection["not-rule"] ?? []).map((one) => `--not-rule ${one}`),
        ...(selection["not-expected"] ?? []).map((one) => `--not-expected ${one}`),
        ...(selection["not-refusal"] ?? []).map((one) => `--not-refusal ${one}`),
        ...(selection["not-outcome"] ?? []).map((one) => `--not-outcome ${one}`)
      ]
      return words.join(" ")
    }
    case "captures":
      return `--capture ${selection.captures.length === 1 ? selection.captures[0] : `${selection.captures.length} captures`}`
    case "universe":
      return [
        `--${selection.universe}`,
        ...(selection.maxSpan === undefined ? [] : [`--max-span ${selection.maxSpan}`]),
        ...(selection.greenOn === undefined ? [] : [`--green-on ${selection.greenOn}`]),
        ...(selection.sample === undefined ? [] : [`--sample ${selection.sample}`]),
        ...(selection.limit === undefined ? [] : [`--limit ${selection.limit}`])
      ].join(" ")
    case "cells":
      return `--cells (${selection.cells.length} ticked)`
  }
}

/**
 * The request line the panel shows: the deployment's request command
 * (`Commands.request`), the lanes picked as the request's `--lane` (one
 * request each), then the selection — a filter states the bar's lane too, so
 * the line reads as the command it stands for.
 */
export const createLine = (command: string, picked: ReadonlyArray<string>, selection: RequestSelection): string =>
  `${command} --lane ${picked.length === 0 ? "<lane>" : picked.join("|")} ${describeSelection(selection)}`

/** The knobs as typed: "" = the default. */
export interface KnobsInput {
  readonly concurrency: string
  readonly batchSize: string
  readonly memoryMax: string
  readonly startsPerSecond: string
  readonly cleanup: boolean
}

export const NO_KNOBS: KnobsInput = { concurrency: "", batchSize: "", memoryMax: "", startsPerSecond: "", cleanup: true }

/** The knobs a request is created with: what was typed, over the defaults. */
export const knobsOf = (input: KnobsInput, defaults: RequestKnobs): RequestKnobs => ({
  concurrency: Math.max(1, Math.floor(number(input.concurrency) ?? defaults.concurrency)),
  batchSize: Math.max(1, Math.floor(number(input.batchSize) ?? defaults.batchSize)),
  memoryMax: blank(input.memoryMax.trim()) ?? defaults.memoryMax,
  startsPerSecond: Math.max(0, number(input.startsPerSecond) ?? defaults.startsPerSecond),
  cleanup: input.cleanup
})

/** What a lane's ceilings say of the knobs: one line per clamp the lane will apply, none where none. */
export const ceilingNotes = (knobs: RequestKnobs, lane: LaneView): ReadonlyArray<string> => [
  ...(lane.cellCeiling !== null && knobs.concurrency > lane.cellCeiling
    ? [`${lane.lane} clamps concurrency to ${lane.cellCeiling}`]
    : []),
  ...(lane.startCeiling !== null && (knobs.startsPerSecond === 0 || knobs.startsPerSecond > lane.startCeiling)
    ? [`${lane.lane} paces starts at ${lane.startCeiling}/s`]
    : []),
  ...(lane.exclusive ? [`${lane.lane} runs one replay at a time; a second waits by name`] : [])
]

/** The preview's one line for a lane: cells, captures, and the rows no cut names a key for. */
export const previewLine = (preview: RequestLanePreview): string => {
  const full = preview.captures.filter((one) => one.kind === "full").length
  return `${preview.cells} cell(s) over ${preview.captures.length} capture(s) — ${full} full, ` +
    `${preview.captures.length - full} as cases` +
    (preview.unkeyed === 0 ? "" : `; ${preview.unkeyed} row(s) no cut names a key for`)
}

/** A lane's captures as the pane lists them: by name. */
export const listedCaptures = (preview: RequestLanePreview): ReadonlyArray<RequestPreviewCapture> =>
  [...preview.captures].sort((a, b) => a.capture.localeCompare(b.capture))

/** Past this many rows a lane's capture list folds behind its count. */
export const FOLD_CAPTURES = 20

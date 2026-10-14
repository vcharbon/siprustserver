/**
 * A replay REQUEST from a selection: the triage filter bar's words, a capture
 * list meant whole, the universe with the population predicates, or a file of
 * ticked cells — resolved server-side, with no row cap, into the (capture,
 * call key) cells the store freezes as one insert.
 *
 * Nothing here runs anything: the batch that replays a request is the
 * replay's. What this owns is which calls a selection names, and the lines a
 * session reads a request and its resolution by.
 */
import { Calls, Cuts, Facets, Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type { Paths } from "../paths.js"
import * as Population from "../population.js"
import { isPartial, type Selection as UniverseSelection, workList } from "../universe.js"

/** The selection a command line or a page stated, before it is resolved. */
export type SelectionInput =
  /** The bar's words, read on `lane`'s queue — the request's own lane where none is named. */
  | { readonly kind: "filter"; readonly query: Facets.Query; readonly lane?: string | undefined }
  | { readonly kind: "captures"; readonly captures: ReadonlyArray<string> }
  | { readonly kind: "universe"; readonly selection: UniverseSelection }
  | { readonly kind: "cells"; readonly file: string }
  /** Cells handed over as they are — the rows a page ticked. */
  | { readonly kind: "ticked"; readonly cells: ReadonlyArray<Requests.Cell> }

/** The file a ticked list is recorded as: the page is the file. */
export const TICKED = "<ticked>"

/** Cells per batch: what one driver boot carries whatever the captures. */
export const DEFAULT_BATCH_CELLS = 1000

/** What a selection came to: the cells, the selection as the row keeps it, and what it could not name. */
export interface Frozen {
  readonly selection: Requests.Selection
  readonly cells: ReadonlyArray<Requests.Cell>
  /** Rows the filter matched that no cut names a key for; they are on no request. */
  readonly unkeyed: ReadonlyArray<{ readonly capture: string; readonly caseId: string }>
}

/** Why the selection cannot be frozen, as the line a session reads. */
export class Refused {
  readonly _tag = "Request.Refused"
  constructor(readonly message: string) {}
}

const blank = (value: string | undefined): boolean => value === undefined || value === ""

const excludes = (query: Facets.Query): Readonly<Partial<Record<string, ReadonlyArray<string>>>> =>
  Object.fromEntries(
    Object.entries(query.exclude ?? {}).filter(([, values]) => values !== undefined && values.length > 0)
  )

/**
 * Whether a filter means whole captures: scope `all` with no chip — included
 * or excluded — selects every call of every capture it matches, so those
 * captures are meant whole.
 */
const filterIsWhole = (query: Facets.Query): boolean =>
  query.scope === "all" && blank(query.rule) && blank(query.expected) && blank(query.refusal) &&
  blank(query.outcome) && Object.keys(excludes(query)).length === 0

const distinctCaptures = (cells: ReadonlyArray<Requests.Cell>): ReadonlyArray<string> =>
  [...new Set(cells.map((cell) => cell.capture))].sort()

/** `capture<ws>key` per line, blank lines and `#` comments skipped; a line short of two words refuses the file. */
export const parseCells = (text: string, file: string): ReadonlyArray<Requests.Cell> | Refused => {
  const cells: Array<Requests.Cell> = []
  const lines = text.split("\n")
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index]!.trim()
    if (line === "" || line.startsWith("#")) continue
    const words = line.split(/\s+/)
    if (words.length < 2) return new Refused(`${file}:${index + 1}: expected "<capture> <call key>", got ${JSON.stringify(line)}`)
    cells.push({ capture: words[0]!, callKey: words[1]! })
  }
  return cells
}

/**
 * The bar's rows on ONE queue: `onLane`'s where the bar named it, the
 * request's own otherwise. A filter is the bar as it stood, and the bar
 * stands on a lane — one lane's red calls requested on another are the
 * keys the first lane's queue holds, while the request's lane stays the target.
 * The selection keeps the queue's lane only where it is not the request's.
 */
const filter = Effect.fn("Request.filter")(function*(lane: string, query: Facets.Query, onLane: string | undefined) {
  const calls = yield* Calls.Service
  const queue = onLane === undefined || onLane === "" ? lane : onLane
  const rows = (yield* calls.queue(queue, query, 0)).rows
  const cells: Array<Requests.Cell> = []
  const unkeyed: Array<{ capture: string; caseId: string }> = []
  for (const row of rows) {
    if (row.callKey === null) unkeyed.push({ capture: row.capture, caseId: row.caseId })
    else cells.push({ capture: row.capture, callKey: row.callKey })
  }
  const selection: Requests.Selection = {
    kind: "filter",
    // Recorded only where it is not the request's own: the sentence names what differs.
    ...(queue === lane ? {} : { lane: queue }),
    scope: query.scope ?? Facets.DEFAULT_SCOPE,
    ...(blank(query.capture) ? {} : { q: query.capture }),
    ...(blank(query.rule) ? {} : { rule: query.rule }),
    ...(blank(query.expected) ? {} : { expected: query.expected }),
    ...(blank(query.refusal) ? {} : { refusal: query.refusal }),
    ...(blank(query.outcome) ? {} : { outcome: query.outcome }),
    ...(Object.keys(excludes(query)).length === 0 ? {} : { exclude: excludes(query) }),
    whole: filterIsWhole(query) ? distinctCaptures(cells) : []
  }
  return { selection, cells, unkeyed } satisfies Frozen
})

const wholeCaptures = Effect.fn("Request.captures")(function*(prepId: number, named: ReadonlyArray<string>) {
  const cuts = yield* Cuts.Service
  const wanted = [...new Set(named)].sort()
  const cut = yield* cuts.cases(prepId, wanted)
  // A capture no prep at the ceiling cut is a typo until proven otherwise:
  // frozen, it would be a whole capture of nothing that never grows.
  const unknown = wanted.filter((capture) => !cut.has(capture))
  if (unknown.length > 0) {
    return new Refused(`no cut as of prep ${prepId} holds ${unknown.map((one) => JSON.stringify(one)).join(", ")}`)
  }
  const cells = wanted.flatMap((capture) =>
    (cut.get(capture) ?? []).map((one) => ({ capture, callKey: one.callKey } satisfies Requests.Cell))
  )
  return { selection: { kind: "captures", captures: wanted, whole: wanted }, cells, unkeyed: [] } satisfies Frozen
})

const universe = Effect.fn("Request.universe")(function*(
  paths: Paths,
  lane: string,
  prepId: number,
  selection: UniverseSelection
) {
  const refused = yield* Population.refusal(paths, selection)
  if (refused !== null) return new Refused(refused)
  const cuts = yield* Cuts.Service
  const work = yield* workList(paths, lane, selection, prepId)
  const cut = yield* cuts.cases(prepId, work.map((one) => one.capture))
  const cells: Array<Requests.Cell> = []
  for (const one of work) {
    const cases = cut.get(one.capture) ?? []
    const kept = isPartial(one) ? new Set(one.set.cases) : undefined
    for (const entry of cases) {
      if (kept !== undefined && !kept.has(entry.caseId)) continue
      cells.push({ capture: one.capture, callKey: entry.callKey })
    }
  }
  return {
    selection: {
      kind: "universe",
      universe: selection.universe === "standing" ? "standing" : "all",
      ...(selection.maxSpanSeconds === undefined ? {} : { maxSpan: selection.maxSpanSeconds }),
      ...(selection.greenOnLane === undefined ? {} : { greenOn: selection.greenOnLane }),
      ...(selection.sampleSize === undefined ? {} : { sample: selection.sampleSize }),
      ...(selection.limit === undefined ? {} : { limit: selection.limit }),
      // A capture the span bound kept in part is the calls under the bound, not the capture.
      whole: work.filter((one) => !isPartial(one)).map((one) => one.capture)
    },
    cells,
    unkeyed: []
  } satisfies Frozen
})

const cellsFile = Effect.fn("Request.cellsFile")(function*(file: string) {
  const fs = yield* FileSystem.FileSystem
  const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => undefined))
  if (text === undefined) return new Refused(`${file}: no such file`)
  const cells = parseCells(text, file)
  if (cells instanceof Refused) return cells
  return { selection: { kind: "cells", file, whole: [] }, cells, unkeyed: [] } satisfies Frozen as Frozen | Refused
})

/**
 * The cells a selection names, as of `prepId`: the filter over its lane's
 * queue (the request's own unless the bar named one), the named captures'
 * cuts, the universe's work list, or the file as given. The filter and the file know no prep; a whole-capture form reads the
 * cut standing at the ceiling, exactly as a replay pinned to it would.
 */
export const freeze = Effect.fn("Request.freeze")(function*(
  paths: Paths,
  lane: string,
  prepId: number,
  input: SelectionInput
) {
  switch (input.kind) {
    case "filter":
      return yield* filter(lane, input.query, input.lane)
    case "captures":
      return yield* wholeCaptures(prepId, input.captures)
    case "universe":
      return yield* universe(paths, lane, prepId, input.selection)
    case "cells":
      return yield* cellsFile(input.file)
    case "ticked":
      return { selection: { kind: "cells", file: TICKED, whole: [] }, cells: input.cells, unkeyed: [] } satisfies Frozen
  }
})

/**
 * The selection WIDENED to these captures whole ("run those whole
 * first"): every key of each one's cut at `prepId` joins the cells, and the
 * capture joins `whole`, so the request resolves `full` for it and grows with
 * a re-cut. A capture no cut holds is refused as a whole capture is.
 */
export const widen = Effect.fn("Request.widen")(function*(
  frozen: Frozen,
  prepId: number,
  captures: ReadonlyArray<string>
) {
  if (captures.length === 0) return frozen
  const grown = yield* wholeCaptures(prepId, captures)
  if (grown instanceof Refused) return grown
  const keyOf = (cell: Requests.Cell): string => `${cell.capture}\u0000${cell.callKey}`
  const held = new Set(frozen.cells.map(keyOf))
  const cells = [...frozen.cells, ...grown.cells.filter((cell) => !held.has(keyOf(cell)))]
  const whole = [...new Set([...frozen.selection.whole, ...grown.selection.whole])].sort()
  return { ...frozen, selection: { ...frozen.selection, whole }, cells } satisfies Frozen
})

/** One capture of a preview: what its cells come to on the lane before anything is written. */
export interface PreviewCapture {
  readonly capture: string
  readonly cells: number
  /** The keys its cut holds at the prep: what the capture comes to whole, `cells` once widened to it. */
  readonly cut: number
  /** `full` when the cells are every key of the cut at the prep, `cases` otherwise. */
  readonly kind: "full" | "cases"
}

export interface Preview {
  readonly prepId: number
  readonly cells: number
  readonly captures: ReadonlyArray<PreviewCapture>
  /** The captures the selection meant whole. */
  readonly whole: ReadonlyArray<string>
  /** Rows the filter matched that no cut names a key for. */
  readonly unkeyed: number
}

/**
 * What a frozen selection comes to against `prepId`, read-only:
 * `Requests.resolve`'s full/cases rule over the cells as they stand (a whole
 * capture counted as its cut, since the resolution would grow it).
 */
export const preview = Effect.fn("Request.preview")(function*(prepId: number, frozen: Frozen) {
  const cuts = yield* Cuts.Service
  const whole = new Set(frozen.selection.whole)
  const held = new Map<string, Set<string>>()
  for (const cell of frozen.cells) {
    const keys = held.get(cell.capture) ?? new Set<string>()
    keys.add(cell.callKey)
    held.set(cell.capture, keys)
  }
  const names = [...new Set([...held.keys(), ...whole])].sort()
  const cut = yield* cuts.cases(prepId, names)
  const captures: Array<PreviewCapture> = []
  for (const capture of names) {
    const keys = held.get(capture) ?? new Set<string>()
    const cases = cut.get(capture) ?? []
    const full = whole.has(capture) || (cases.length > 0 && cases.every((one) => keys.has(one.callKey)))
    captures.push({ capture, cells: keys.size, cut: cases.length, kind: full ? "full" : "cases" })
  }
  return {
    prepId,
    cells: frozen.cells.length,
    captures,
    whole: [...whole].sort(),
    unkeyed: frozen.unkeyed.length
  } satisfies Preview
})

/** A capture list as one line: the first few, then how many more. */
const namedCaptures = (captures: ReadonlyArray<string>, shown = 5): string =>
  captures.length <= shown
    ? captures.join(", ")
    : `${captures.slice(0, shown).join(", ")} +${captures.length - shown} more`

/** The selection as the sentence it was stated as. */
export const describe = (selection: Requests.Selection): string => {
  switch (selection.kind) {
    case "filter": {
      const chips = [
        ...(selection.lane === undefined ? [] : [`--on-lane ${selection.lane}`]),
        `--scope ${selection.scope}`,
        ...(selection.q === undefined ? [] : [`--q ${selection.q}`]),
        ...(selection.rule === undefined ? [] : [`--rule ${selection.rule}`]),
        ...(selection.expected === undefined ? [] : [`--expected ${selection.expected}`]),
        ...(selection.refusal === undefined ? [] : [`--refusal ${selection.refusal}`]),
        ...(selection.outcome === undefined ? [] : [`--outcome ${selection.outcome}`]),
        ...Object.entries(selection.exclude ?? {}).flatMap(([group, values]) =>
          (values ?? []).map((value) => `--not-${group} ${value}`)
        )
      ]
      return chips.join(" ")
    }
    case "captures":
      return `--capture ${namedCaptures(selection.captures)}`
    case "universe":
      return [
        `--${selection.universe}`,
        ...(selection.maxSpan === undefined ? [] : [`--max-span ${selection.maxSpan}`]),
        ...(selection.greenOn === undefined ? [] : [`--green-on ${selection.greenOn}`]),
        ...(selection.sample === undefined ? [] : [`--sample ${selection.sample}`]),
        ...(selection.limit === undefined ? [] : [`--limit ${selection.limit}`])
      ].join(" ")
    case "cells":
      return `--cells ${selection.file}`
  }
}

/** The preview, one line per capture: what `create --dry-run` prints. */
export const previewLines = (shown: Preview, lane: string): ReadonlyArray<string> => {
  const full = shown.captures.filter((one) => one.kind === "full").length
  return [
    `${shown.cells} cell(s) over ${shown.captures.length} capture(s) on ${lane} against prep ${shown.prepId} — ` +
    `${full} full, ${shown.captures.length - full} as cases` +
    (shown.unkeyed === 0 ? "" : `; ${shown.unkeyed} row(s) no cut names a key for`),
    ...shown.captures.map((one) => `  ${one.capture}  ${one.kind.padEnd(5)}  ${one.cells} cell(s)`)
  ]
}

export const createdLine = (created: Requests.Created, lane: string): string =>
  `request ${created.id}: ${created.cells} cell(s) over ${created.captures.length} capture(s) on ${lane}` +
  (created.captures.length === 0 ? "" : ` — ${namedCaptures(created.captures)}`)

export const requestLine = (row: Requests.Row): string =>
  `request ${String(row.id).padStart(4)}  ${row.createdAt}  ${row.lane}  ${describe(row.selection)}  ` +
  `${row.cells} cell(s) / ${row.captures} capture(s)  ${row.jobs} job(s)  by ${row.createdBy}`

const goneLine = (cell: Requests.GoneCell): string =>
  `    gone ${cell.callKey}: ${cell.gone === "refused" ? `refused ${cell.reasons.join(", ")}` : "not in this cut"}`

/**
 * The resolution narrowed to a RESUME: the planned cells no
 * job of the request ran, every capture a `cases` set — a partial re-run is
 * never a whole one — and the gone cells as resolved, since a job records
 * them either way. A capture with nothing left to run is dropped.
 */
export const unranOnly = (
  resolution: Requests.Resolution,
  unran: ReadonlyArray<Requests.Cell>
): Requests.Resolution => {
  const keyOf = (cell: Requests.Cell): string => `${cell.capture}\u0000${cell.callKey}`
  const left = new Set(unran.map(keyOf))
  return {
    ...resolution,
    captures: resolution.captures.flatMap((capture) => {
      const planned = capture.planned.filter((cell) => left.has(keyOf(cell)))
      const gone = capture.gone.filter((cell) => left.has(keyOf(cell)))
      return planned.length === 0 && gone.length === 0 ? [] : [{ ...capture, kind: "cases" as const, planned, gone }]
    })
  }
}

/** The resolution, one line per capture and one per gone or added key. */
export const resolutionLines = (resolution: Requests.Resolution): ReadonlyArray<string> => {
  const planned = resolution.captures.reduce((sum, one) => sum + one.planned.length, 0)
  const gone = resolution.captures.reduce((sum, one) => sum + one.gone.length, 0)
  const added = resolution.captures.reduce((sum, one) => sum + one.added.length, 0)
  const full = resolution.captures.filter((one) => one.kind === "full").length
  const lines = [
    `request ${resolution.requestId} against prep ${resolution.prepId} on ${resolution.lane}: ` +
    `${resolution.captures.length} capture(s) — ${full} full, ${resolution.captures.length - full} as cases; ` +
    `${planned} planned, ${gone} gone, ${added} added by the prep`
  ]
  for (const one of resolution.captures) {
    lines.push(
      `  ${one.capture}  ${one.kind.padEnd(5)}  ${one.planned.length} planned  ${one.gone.length} gone  ${one.added.length} added`
    )
    for (const cell of one.gone) lines.push(goneLine(cell))
    for (const key of one.added) lines.push(`    added ${key}`)
  }
  return lines
}

/** The request, its cells per capture by origin, and its jobs. */
export const detailLines = (detail: Requests.Detail): ReadonlyArray<string> => {
  const lines = [requestLine(detail.request), `  knobs ${JSON.stringify(detail.request.knobs)}`]
  const perCapture = new Map<string, { selected: number; added: number }>()
  for (const cell of detail.cells) {
    const counts = perCapture.get(cell.capture) ?? { selected: 0, added: 0 }
    if (cell.origin === "selected") counts.selected += 1
    else counts.added += 1
    perCapture.set(cell.capture, counts)
  }
  for (const [capture, counts] of perCapture) {
    lines.push(`  ${capture}  ${counts.selected} selected  ${counts.added} added by prep`)
  }
  if (detail.jobs.length === 0) lines.push("  no job yet")
  for (const job of detail.jobs) {
    lines.push(`  job ${job.jobId}  ${job.planned} planned  ${job.ran} ran  ${job.gone} gone`)
  }
  return lines
}

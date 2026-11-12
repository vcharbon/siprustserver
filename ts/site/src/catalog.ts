/**
 * The catalog: what the configured roots hold, and how to read one entry.
 *
 * A case root holds either case DIRECTORIES (`<case-id>/scenario.json` plus
 * `resources/`, the layout the pivot spec states) or bare `<name>.json`
 * documents, which is how sample and fixture sets are kept. Both are listed
 * the same way, so a reviewer never has to know which shape a root uses.
 *
 * A third shape is a case directory the generator REFUSED to write: it holds
 * `excluded.json` and no scenario. It is listed like the others and carries its
 * refusal, because the evidence for a ruling is the case that was not made.
 *
 * A run root holds run bundles: `pivot.json`, `verdict.json`, `timing.json`,
 * `run-config.json` and `recording/<leg>.jsonl`.
 */
import { captureSelection, ladderWindow, legWindows, servedWindow } from "@sip/viewer-core/flows"
import { pivotOf } from "@sip/corpus/report/call-ladders"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { ViewerConfig } from "./config.ts"
import { type MockAnswers, mockAnswersOf } from "./ledger-view.ts"
import * as FlowsWalk from "./flows-walk.ts"

/** Separates the root id from the entry id inside one URL path segment. */
export const ID_SEPARATOR = "~"

/**
 * The generator's refusal to cut this case. Present only where the directory
 * holds NO scenario: an `excluded.json` beside a scenario belongs to a sibling
 * case of the same capture, and does not make this one excluded.
 */
export interface CaseExclusion {
  readonly path: string
  /** `excluded.json` verbatim — the generator owns that shape, not this file. */
  readonly document: unknown
}

export interface CaseSummary {
  readonly id: string
  readonly rootId: string
  readonly caseId: string
  /**
   * The document's own `case.id`. It is the join key between a case and the
   * runs that replayed it: entry names differ from it (`alt-answered.v3` holds
   * `alt-answered`), so a name match would miss real pairs.
   */
  readonly caseKey: string
  readonly title: string
  readonly family: string
  readonly origin: string
  readonly variant: string
  readonly lanes: Readonly<Record<string, string>>
  readonly path: string
  /**
   * Sibling documents in the case directory (`scenario`, `scenario.target`, …).
   * EMPTY for an excluded entry: there is nothing to draw, compare or run.
   */
  readonly variants: readonly string[]
  /** Why no case was generated. ABSENT on a generated case, never null. */
  readonly excluded?: CaseExclusion
}

export interface CaseDetail extends CaseSummary {
  readonly document: unknown
  readonly resources: readonly string[]
  /**
   * The case's ledger row — the answers the backend mock serves on its call
   * and failure routes — as the viewer's mock answers (`ledger-view.ts`). Null
   * where the case dir holds none: every refused case and every hand-authored one.
   */
  readonly ledger: MockAnswers | null
}

/** One window of a source document's legs, at the indices the document gives them. */
export interface CaseSourceWindow {
  readonly from: number
  readonly legs: readonly unknown[]
}

/**
 * The capture a generated case was cut from. Absence is an ANSWER, not a
 * failure: an authored case has no source, and a captured one whose flows
 * document was not kept says so rather than erroring.
 */
export interface CaseSource {
  readonly available: boolean
  /** The capture the case names, verbatim. */
  readonly capture: string | undefined
  /** The call groups the case spans, as indices into the document's groups. */
  readonly callGroups: readonly number[]
  readonly path: string | undefined
  /** The document with its `legs` emptied: no response carries a big capture's whole. */
  readonly document: unknown
  /** Legs the document holds, however few this answer carries. */
  readonly legs: number
  /** The window the pane opens on: the legs the case is about, else the first. */
  readonly window: CaseSourceWindow
  /**
   * The legs the case cites that lie past that window — a call dialled long
   * after its first leg on a busy capture — one window per run of them, so the
   * pane opens on EVERY leg the case is about whatever their distance.
   */
  readonly cited: readonly CaseSourceWindow[]
  /** Why nothing was found, when nothing was. */
  readonly detail: string
}

export interface RunSummary {
  readonly id: string
  readonly rootId: string
  readonly runId: string
  /** The `case.id` of the bundle's `pivot.json`: the case this run replayed. */
  readonly caseKey: string
  readonly path: string
  readonly legs: readonly string[]
  readonly verdict: unknown
}

export interface RunDetail extends RunSummary {
  readonly pivot: unknown
  readonly timing: unknown
  readonly runConfig: unknown
  /** Verbatim `recording/<leg>.jsonl` contents, keyed by leg. */
  readonly recording: Readonly<Record<string, string>>
}

/**
 * What a route was asked for and holds nothing of; `accepted` names the form
 * the route takes the id in, so a reader holding a cited id knows how to ask.
 */
export class NotFound extends Schema.TaggedError<NotFound>()("Site.NotFound", {
  id: Schema.String,
  accepted: Schema.optional(Schema.String),
  /** What the store's resolver says of the id, where it was asked: the forms it reads, or what the id is. */
  note: Schema.optional(Schema.String)
}) {}

export interface Interface {
  readonly listCases: Effect.Effect<readonly CaseSummary[]>
  readonly readCase: (id: string, variant?: string | undefined) => Effect.Effect<CaseDetail, NotFound>
  readonly readCaseSource: (id: string, variant?: string | undefined) => Effect.Effect<CaseSource, NotFound>
  /** A further window of the case's source document; a case with no source is a 404. */
  readonly readCaseSourceLegs: (
    id: string,
    variant: string | undefined,
    from: number,
    count?: number | undefined
  ) => Effect.Effect<CaseSourceWindow, NotFound>
  readonly listRuns: Effect.Effect<readonly RunSummary[]>
  readonly readRun: (id: string) => Effect.Effect<RunDetail, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Catalog") {}

interface CaseLocation {
  readonly rootId: string
  readonly caseId: string
  /** Directory holding the document (the case dir, or the root for a bare file). */
  readonly dir: string
  /** Document path per variant name. Empty where the case was refused. */
  readonly documents: Readonly<Record<string, string>>
  /** `excluded.json`, where the directory holds one. */
  readonly excludedAt: string | undefined
}

const caseBlock = (document: unknown): Record<string, unknown> => {
  if (typeof document !== "object" || document === null) return {}
  const block = (document as Record<string, unknown>)["case"]
  return typeof block === "object" && block !== null ? block as Record<string, unknown> : {}
}

const asString = (value: unknown, fallback: string): string =>
  typeof value === "string" ? value : fallback

const FLOWS_SUFFIX = ".flows.json"
const ANON_FLOWS_SUFFIX = ".anon.flows.json"

/**
 * The capture a file or a `case.source.capture` value names, with the extractor
 * and capture suffixes taken off, so both sides of the match normalise the same.
 */
const captureKey = (name: string): string => {
  let key = name.endsWith(FLOWS_SUFFIX) ? name.slice(0, -FLOWS_SUFFIX.length) : name
  if (key.endsWith(".anon")) key = key.slice(0, -".anon".length)
  for (const suffix of [".pcapng.gz", ".pcap.gz", ".pcapng", ".pcap", ".gz"]) {
    if (key.endsWith(suffix)) {
      key = key.slice(0, -suffix.length)
      break
    }
  }
  return key
}

/**
 * The flows document whose capture key prefixes `caseId`, longest key first so
 * `capture_204` never answers for `capture_2040`. This is how an EXCLUDED entry
 * finds its capture: it carries no document to name one, and the generator
 * guarantees a case id starts with the capture it was cut from.
 */
const matchByPrefix = <A>(
  flows: ReadonlyMap<string, A>,
  caseId: string
): A | undefined => {
  let best: { key: string; value: A } | undefined
  for (const [key, value] of flows) {
    if (!caseId.startsWith(key)) continue
    if (best === undefined || key.length > best.key.length) best = { key, value }
  }
  return best?.value
}

const numbers = (value: unknown): readonly number[] =>
  Array.isArray(value) ? value.filter((one): one is number => typeof one === "number") : []

/**
 * What an `excluded.json` says the pane is about: the groups and the legs its
 * records name, the record being the cut's on-disk contract for both. A record
 * naming neither leaves the pane on the document's first legs.
 */
const excludedSpan = (
  document: unknown
): { readonly groups: readonly number[]; readonly legs: readonly number[] } => {
  const groups = new Set<number>()
  const legs = new Set<number>()
  for (const entry of Array.isArray(document) ? document : []) {
    if (typeof entry !== "object" || entry === null) continue
    const record = entry as Record<string, unknown>
    for (const group of numbers(record["groups"])) groups.add(group)
    for (const leg of numbers(record["legs"])) legs.add(leg)
    for (const one of Array.isArray(record["evidence"]) ? record["evidence"] : []) {
      const leg = typeof one === "object" && one !== null ? (one as Record<string, unknown>)["leg"] : undefined
      if (typeof leg === "number") legs.add(leg)
    }
  }
  const ascending = (set: ReadonlySet<number>) => [...set].sort((a, b) => a - b)
  return { groups: ascending(groups), legs: ascending(legs) }
}

/**
 * Indices of the envelope's groups holding any of `legs`. It reads the groups
 * alone, so a document whose legs no response carries still narrows.
 */
const groupsOfLegs = (envelope: unknown, legs: readonly number[]): readonly number[] => {
  if (legs.length === 0 || typeof envelope !== "object" || envelope === null) return []
  const wanted = new Set(legs)
  const groups = (envelope as Record<string, unknown>)["groups"]
  return (Array.isArray(groups) ? groups : []).flatMap((group, index) => {
    const members = typeof group === "object" && group !== null
      ? (group as Record<string, unknown>)["legs"]
      : undefined
    if (!Array.isArray(members)) return []
    return members.some((leg) => typeof leg === "number" && wanted.has(leg)) ? [index] : []
  })
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service
    // One walk of a flows file answers the source pane and every window after
    // it, for as long as that file stands unchanged.
    const walks = FlowsWalk.make()

    /**
     * A document off disk, or `null` where the file is missing or is not JSON.
     * Absence is a VALUE here: every read below crosses the wire as JSON, which
     * carries a null and drops an undefined.
     */
    const readJson = Effect.fn("Catalog.readJson")(function*(file: string) {
      const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => ""))
      if (text === "") return null
      return yield* Effect.try({ try: () => JSON.parse(text) as unknown, catch: () => null }).pipe(
        Effect.orElseSucceed(() => null)
      )
    })

    /**
     * The case's ledger row: the answers the mock serves for this case, as the
     * cut wrote them. One row per case dir, named by the ledger number, so the
     * first `.json` under `ledger/` IS it. `undefined` where the dir holds none.
     */
    const readLedgerRow = Effect.fn("Catalog.readLedgerRow")(function*(dir: string) {
      const ledgerDir = path.join(dir, "ledger")
      const entries = yield* fs.readDirectory(ledgerDir).pipe(
        Effect.orElseSucceed(() => [] as string[])
      )
      const row = entries.sort().find((entry) => entry.endsWith(".json"))
      return row === undefined ? null : mockAnswersOf(yield* readJson(path.join(ledgerDir, row)))
    })

    const locateCases = Effect.fn("Catalog.locateCases")(function*() {
      const found: CaseLocation[] = []
      for (const root of config.caseRoots) {
        const entries = yield* fs.readDirectory(root.path).pipe(Effect.orElseSucceed(() => [] as string[]))
        for (const entry of entries.sort()) {
          const full = path.join(root.path, entry)
          const info = yield* fs.stat(full).pipe(Effect.orElseSucceed(() => undefined))
          if (info === undefined) continue
          if (info.type === "Directory") {
            const inner = yield* fs.readDirectory(full).pipe(Effect.orElseSucceed(() => [] as string[]))
            const documents: Record<string, string> = {}
            let excludedAt: string | undefined
            for (const name of inner) {
              if (name === "scenario.json") documents["scenario"] = path.join(full, name)
              else if (name === "scenario.target.json") documents["scenario.target"] = path.join(full, name)
              else if (name === "excluded.json") excludedAt = path.join(full, name)
            }
            if (Object.keys(documents).length > 0 || excludedAt !== undefined) {
              found.push({ rootId: root.id, caseId: entry, dir: full, documents, excludedAt })
            }
          } else if (entry.endsWith(".json")) {
            const caseId = entry.slice(0, -".json".length)
            found.push({
              rootId: root.id,
              caseId,
              dir: root.path,
              documents: { scenario: full },
              excludedAt: undefined
            })
          }
        }
      }
      return found
    })

    const summarise = Effect.fn("Catalog.summarise")(function*(location: CaseLocation) {
      const primary = location.documents["scenario"] ?? Object.values(location.documents)[0]
      const document = primary === undefined ? null : yield* readJson(primary)
      // Only a directory with no scenario at all is excluded; see `CaseExclusion`.
      const excluded = primary !== undefined || location.excludedAt === undefined
        ? undefined
        : { path: location.excludedAt, document: yield* readJson(location.excludedAt) }
      const block = caseBlock(document)
      const lanes = typeof block["lanes"] === "object" && block["lanes"] !== null
        ? block["lanes"] as Record<string, string>
        : {}
      return {
        id: `${location.rootId}${ID_SEPARATOR}${location.caseId}`,
        rootId: location.rootId,
        caseId: location.caseId,
        caseKey: asString(block["id"], location.caseId),
        title: asString(block["title"], location.caseId),
        family: asString(block["family"], "unknown"),
        // A refusal is only ever written for a case cut from a capture, so the
        // origin of an excluded entry is known even though no document states it.
        origin: asString(block["origin"], excluded === undefined ? "unknown" : "capture"),
        variant: asString(block["variant"], "unknown"),
        lanes,
        path: primary ?? location.excludedAt ?? location.dir,
        variants: Object.keys(location.documents).sort(),
        ...(excluded === undefined ? {} : { excluded })
      } satisfies CaseSummary
    })

    const listCases = Effect.gen(function*() {
      const locations = yield* locateCases()
      return yield* Effect.forEach(locations, summarise)
    })

    const readCase = Effect.fn("Catalog.readCase")(function*(id: string, variant?: string | undefined) {
      const locations = yield* locateCases()
      const location = locations.find(
        (entry) => `${entry.rootId}${ID_SEPARATOR}${entry.caseId}` === id
      )
      if (location === undefined) return yield* new NotFound({ id })
      const wanted = variant ?? "scenario"
      const file = location.documents[wanted] ?? location.documents["scenario"]
      const summary = yield* summarise(location)
      if (file === undefined) {
        // An excluded entry HAS no scenario document, and that is its answer:
        // the refusal it carries is what the page renders, beside the capture.
        if (summary.excluded !== undefined) {
          return { ...summary, document: null, resources: [], ledger: null } satisfies CaseDetail
        }
        return yield* new NotFound({ id: `${id}#${wanted}` })
      }
      const document = yield* readJson(file)
      const resources = yield* fs.readDirectory(path.join(location.dir, "resources")).pipe(
        Effect.orElseSucceed(() => [] as string[])
      )
      const ledger = yield* readLedgerRow(location.dir)
      return {
        ...summary,
        path: file,
        document,
        resources: resources.sort(),
        ledger
      } satisfies CaseDetail
    })

    /**
     * Flows documents, keyed by capture. A root holds them flat or one
     * directory deep, which is how an extraction run lays its output out.
     * Each entry keeps the capture as the extractor NAMES it, because an
     * excluded case has no document to name it from.
     */
    const locateFlows = Effect.fn("Catalog.locateFlows")(function*() {
      const found = new Map<string, { file: string; capture: string }>()
      // Two levels, iteratively: the root itself, then each directory in it.
      const pending: { dir: string; descend: boolean }[] = config.flowsRoots.map((root) => ({
        dir: root.path,
        descend: true
      }))
      while (pending.length > 0) {
        const { dir, descend } = pending.shift()!
        const entries = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as string[]))
        for (const entry of entries.sort()) {
          const full = path.join(dir, entry)
          if (entry.endsWith(FLOWS_SUFFIX)) {
            const key = captureKey(entry)
            const capture = entry.endsWith(ANON_FLOWS_SUFFIX)
              ? entry.slice(0, -ANON_FLOWS_SUFFIX.length)
              : entry.slice(0, -FLOWS_SUFFIX.length)
            if (!found.has(key)) found.set(key, { file: full, capture })
            continue
          }
          if (!descend) continue
          const info = yield* fs.stat(full).pipe(Effect.orElseSucceed(() => undefined))
          if (info?.type === "Directory") pending.push({ dir: full, descend: false })
        }
      }
      return found
    })

    /**
     * A case's capture source, LOCATED: the flows file walked — never read — and
     * what the pane narrows and opens on. `ok: false` carries the answer the
     * pane draws instead, which is an answer and not a failure.
     */
    const caseFlows = Effect.fn("Catalog.caseFlows")(function*(
      id: string,
      variant?: string | undefined
    ) {
      const detail = yield* readCase(id, variant)
      const block = caseBlock(detail.document)
      const source = typeof block["source"] === "object" && block["source"] !== null
        ? block["source"] as Record<string, unknown>
        : undefined
      const stated = typeof source?.["capture"] === "string" ? source["capture"] : undefined
      const absent = (capture: string | undefined, callGroups: readonly number[], why: string) =>
        ({
          ok: false,
          source: {
            available: false,
            capture,
            callGroups,
            path: undefined,
            document: null,
            legs: 0,
            window: { from: 0, legs: [] },
            cited: [],
            detail: why
          } satisfies CaseSource
        }) as const
      if (config.flowsRoots.length === 0) return absent(stated, [], "no flowsRoots are configured")
      const flows = yield* locateFlows()

      if (detail.excluded !== undefined) {
        // No document states the capture, so the ENTRY NAME does: the generator
        // gives a case the id `<capture>-<…>` and refuses any other.
        const found = matchByPrefix(flows, detail.caseId)
        if (found === undefined) {
          return absent(
            undefined,
            [],
            `no flows document under the configured flows roots is named by a prefix of ` +
              `"${detail.caseId}", so the capture this refusal was made on is not here`
          )
        }
        const walked = yield* Effect.sync(() => walks.walk(found.file))
        if (walked === null) return absent(found.capture, [], `${found.file} is not a flows document`)
        const span = excludedSpan(detail.excluded.document)
        return {
          ok: true,
          capture: found.capture,
          path: found.file,
          walked,
          groups: span.groups,
          legs: span.legs
        } as const
      }

      const callGroups = numbers(source?.["call_groups"])
      if (stated === undefined) {
        return absent(stated, callGroups, "the case names no capture: nothing was cut from a flows document")
      }
      const found = flows.get(captureKey(stated))
      if (found === undefined) {
        return absent(
          stated,
          callGroups,
          `no flows document for capture "${stated}" under the configured flows roots`
        )
      }
      const walked = yield* Effect.sync(() => walks.walk(found.file))
      if (walked === null) return absent(stated, callGroups, `${found.file} is not a flows document`)
      const pivot = pivotOf(detail.document)
      return {
        ok: true,
        capture: stated,
        path: found.file,
        walked,
        groups: callGroups,
        legs: pivot === undefined ? [] : [...captureSelection(pivot).legs]
      } as const
    })

    /**
     * The pane's two answers in one: the envelope, and the first window of legs
     * — the ones the case is about. A capture of thousands of calls has a
     * document over V8's string cap, so nothing here holds one.
     */
    const readCaseSource = Effect.fn("Catalog.readCaseSource")(function*(
      id: string,
      variant?: string | undefined
    ) {
      const found = yield* caseFlows(id, variant)
      if (!found.ok) return found.source
      const document = yield* Effect.try({
        try: () => found.walked.envelope(),
        catch: (error) => error as Error
      }).pipe(Effect.orDie)
      const window = ladderWindow(found.walked.legs, found.legs)
      const legs = yield* Effect.try({
        try: () => found.walked.window(window.from, window.count),
        catch: (error) => error as Error
      }).pipe(Effect.orDie)
      const beyond = found.legs.filter((leg) => leg >= window.from + window.count)
      const cited = yield* Effect.forEach(legWindows(found.walked.legs, beyond), (run) =>
        Effect.try({
          try: () => ({ from: run.from, legs: found.walked.window(run.from, run.count) } satisfies CaseSourceWindow),
          catch: (error) => error as Error
        }).pipe(Effect.orDie))
      return {
        available: true,
        capture: found.capture,
        // The groups the case states, else the ones its legs fall in: both are
        // read off the envelope's `groups`, never off the legs it leaves out.
        callGroups: found.groups.length > 0 ? found.groups : groupsOfLegs(document, found.legs),
        path: found.path,
        document,
        legs: found.walked.legs,
        window: { from: window.from, legs },
        cited,
        detail: ""
      } satisfies CaseSource
    })

    /** A further window, as the reader pages: capped and held inside the document. */
    const readCaseSourceLegs = Effect.fn("Catalog.readCaseSourceLegs")(function*(
      id: string,
      variant: string | undefined,
      from: number,
      count?: number | undefined
    ) {
      const found = yield* caseFlows(id, variant)
      if (!found.ok) return yield* new NotFound({ id: `${id}#source` })
      const window = servedWindow(found.walked.legs, from, count)
      return {
        from: window.from,
        legs: yield* Effect.try({
          try: () => found.walked.window(window.from, window.count),
          catch: (error) => error as Error
        }).pipe(Effect.orDie)
      } satisfies CaseSourceWindow
    })

    const locateRuns = Effect.fn("Catalog.locateRuns")(function*() {
      const found: { rootId: string; runId: string; dir: string }[] = []
      for (const root of config.runRoots) {
        const entries = yield* fs.readDirectory(root.path).pipe(Effect.orElseSucceed(() => [] as string[]))
        for (const entry of entries.sort()) {
          const full = path.join(root.path, entry)
          const info = yield* fs.stat(full).pipe(Effect.orElseSucceed(() => undefined))
          if (info?.type !== "Directory") continue
          const hasRecording = yield* fs.exists(path.join(full, "recording")).pipe(
            Effect.orElseSucceed(() => false)
          )
          const hasPivot = yield* fs.exists(path.join(full, "pivot.json")).pipe(
            Effect.orElseSucceed(() => false)
          )
          if (hasRecording || hasPivot) found.push({ rootId: root.id, runId: entry, dir: full })
        }
      }
      return found
    })

    const legFiles = Effect.fn("Catalog.legFiles")(function*(dir: string) {
      const entries = yield* fs.readDirectory(path.join(dir, "recording")).pipe(
        Effect.orElseSucceed(() => [] as string[])
      )
      return entries.filter((entry) => entry.endsWith(".jsonl")).sort()
    })

    const listRuns = Effect.gen(function*() {
      const locations = yield* locateRuns()
      return yield* Effect.forEach(locations, (location) =>
        Effect.gen(function*() {
          const files = yield* legFiles(location.dir)
          const verdict = yield* readJson(path.join(location.dir, "verdict.json"))
          const pivot = yield* readJson(path.join(location.dir, "pivot.json"))
          return {
            id: `${location.rootId}${ID_SEPARATOR}${location.runId}`,
            rootId: location.rootId,
            runId: location.runId,
            caseKey: asString(caseBlock(pivot)["id"], location.runId),
            path: location.dir,
            legs: files.map((file) => file.slice(0, -".jsonl".length)),
            verdict
          } satisfies RunSummary
        }))
    })

    const readRun = Effect.fn("Catalog.readRun")(function*(id: string) {
      const locations = yield* locateRuns()
      const location = locations.find(
        (entry) => `${entry.rootId}${ID_SEPARATOR}${entry.runId}` === id
      )
      if (location === undefined) return yield* new NotFound({ id })
      const files = yield* legFiles(location.dir)
      const recording: Record<string, string> = {}
      for (const file of files) {
        recording[file.slice(0, -".jsonl".length)] = yield* fs
          .readFileString(path.join(location.dir, "recording", file))
          .pipe(Effect.orElseSucceed(() => ""))
      }
      const pivot = yield* readJson(path.join(location.dir, "pivot.json"))
      return {
        id,
        rootId: location.rootId,
        runId: location.runId,
        caseKey: asString(caseBlock(pivot)["id"], location.runId),
        path: location.dir,
        legs: Object.keys(recording),
        verdict: yield* readJson(path.join(location.dir, "verdict.json")),
        pivot,
        timing: yield* readJson(path.join(location.dir, "timing.json")),
        runConfig: yield* readJson(path.join(location.dir, "run-config.json")),
        recording
      } satisfies RunDetail
    })

    return Service.of({ listCases, readCase, readCaseSource, readCaseSourceLegs, listRuns, readRun })
  })
)

export * as Catalog from "./catalog.ts"

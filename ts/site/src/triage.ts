/**
 * The triage catalog: the queue and the per-capture study payload.
 *
 * The queue is a list of CALLS: one row per case of the standing run of each
 * capture on a lane, at the case's own last run, plus one row per call the
 * standing cut refused — each with its main error. The store answers the URL's
 * words as one query (`Calls.queue`): the page, the count and the groups the
 * pure `Facets` count the chips from; only the rows served are then joined to
 * their documents and stat-ed on disk. Neither the verdict registry nor a
 * ticket is read on that path.
 *
 * A request names WHICH lane and HOW MUCH of it: the lanes are separate SUTs and
 * only ever answer for themselves, and a lane read whole is thousands of green
 * calls. The chip counts are taken over the scope, the capture filter and the
 * excludes, never the includes, so an include narrows what is SERVED without
 * hiding what else the same scope holds, and an exclude takes its calls out of
 * every count but its own chip's, which counts what it hides.
 *
 * The study payload is everything known about ONE capture: its corpus
 * extraction artifacts, its registry record — plus the tracker's state of the
 * ticket a pending record names — and every triage-session run bundle, the
 * latter as what each RULED, never what it carried. A capture gains a bundle
 * per sweep and a reader opens one of them, so the recording, the document and
 * `confrontation.ndjson` are served per cell instead (`cell()`). Absence of any
 * part is an answer, not an error: only a capture nothing at all knows is a 404.
 *
 * Asked for ONE CALL (`case`), the study is the same envelope over that call
 * alone: its cells in every run, its case, its results, the standing cut's
 * refusal of it and the generator's record of it — a capture of thousands of
 * calls is no page for the one a reader opened, so nothing of another call is
 * read and the whole-capture rollups (the extraction's failure, each run's `campaign`) stay with
 * the capture study. A call the cut dropped since the run replayed it still
 * answers with its cells; only a call nothing names is a 404, naming both.
 *
 * The CUT STORE is the source of truth for what a capture stands cut as: the
 * cases and the flows document come from the standing `capture_cuts` reading,
 * never from a directory on the tree. A capture the store holds no cut of falls
 * back to the corpus extraction, which states ids without documents.
 */
import { servedWindow } from "@sip/viewer-core/flows"
import type { HttpExchanges } from "@sip/viewer-core/http"
import { Calls, CallSummary, Cuts, Facets, Model, Refusals, Registry as Triage, Runs, Verifications } from "@sip/store"
import * as CallEvidence from "@sip/corpus/report/call-evidence"
import { TERMINATED } from "@sip/corpus/replay/cell-trim"
import * as Content from "@sip/corpus/content/content"
import { excludedFile } from "@sip/corpus/cut/excluded"
import * as Extraction from "@sip/corpus/deployment/extraction"
import { Confrontation, Verification } from "@sip/contracts"
import { TicketStates } from "@sip/lab-host"
import { Effect, Layer, Option } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import * as zlib from "node:zlib"
import { lineViewOf, type VerificationLineView } from "./api.ts"
import { NotFound } from "./catalog.ts"
import { ViewerConfig } from "./config.ts"
import * as FlowsWalk from "./flows-walk.ts"
import { parseHttpExchanges } from "./http-exchanges.ts"
import { type MockAnswers, mockAnswersOf } from "./ledger-view.ts"
import { make as makeMemo } from "./memo.ts"
import { areSegments } from "./segment.ts"
import { SitePack } from "./site-pack.ts"
import { SutEvidence } from "./sut-evidence.ts"
import * as RunDirs from "./run-dirs.ts"

export interface TicketView {
  /** The Ticket ref, as the registry states it. */
  readonly ref: string
  /** Its state on the tracker (`@sip/lab-host` `ticket-states.ts`). */
  readonly state: Triage.TicketState
  /** Why this install has no tracker to read the state off, where it has none: shown in place of the state. */
  readonly unavailable?: string
}

/** What a reader asked the queue for; every field falls back to the default below. */
export interface QueueRequest {
  /** Which SUT's runs to read; absent = the configured lane. */
  readonly lane?: string | undefined
  /** One of `Facets.SCOPES`; anything else reads as the default, `failed`. */
  readonly scope?: string | undefined
  /** Capture-name substring, matched case-insensitively. */
  readonly q?: string | undefined
  /** Exact main-error rule of a case row. */
  readonly rule?: string | undefined
  /** Exact message a died call waited for. */
  readonly expected?: string | undefined
  /** Exact refusal reason of a refused call. */
  readonly refusal?: string | undefined
  /** Exact outcome of a row. */
  readonly outcome?: Facets.Outcome | undefined
  /** One feature the call's document carries. */
  readonly feature?: string | undefined
  /** The rule of the source fault a case carries, passed or failed: information, never an outcome. */
  readonly source?: string | undefined
  /** The values hidden per group, each key repeated per value in the query string. */
  readonly "not-rule"?: ReadonlyArray<string> | undefined
  readonly "not-expected"?: ReadonlyArray<string> | undefined
  readonly "not-refusal"?: ReadonlyArray<string> | undefined
  readonly "not-outcome"?: ReadonlyArray<Facets.Outcome> | undefined
  readonly "not-feature"?: ReadonlyArray<string> | undefined
  readonly "not-source"?: ReadonlyArray<string> | undefined
  /** Rows to serve after filtering; the rest are counted, not sent. */
  readonly limit?: number | undefined
}

/** A lane read whole is thousands of green calls; this is what one page carries. */
export const QUEUE_LIMIT = 300

/** A lane the run database holds runs on, and how much of it. */
export interface LaneView {
  readonly lane: string
  readonly runs: number
  readonly captures: number
}

/** A served call: the store's row, its document's facts, and whether its evidence survives. */
export interface QueueRow extends Calls.CallDetail {
  /** The run's bundle survives on disk; `false` = cleaned, row intact. A refused call has none. */
  readonly bundle: boolean
}

export interface TriageQueue {
  readonly available: boolean
  /** Why nothing is served, when nothing is. */
  readonly detail: string
  /** The lane this answer reads; the configured one where the request named none. */
  readonly lane?: string
  /** Every lane the run database knows, so the reader can move between them. */
  readonly lanes: readonly LaneView[]
  /** The scope the answer was filtered to, echoed. */
  readonly scope: Facets.Scope
  /** Rows the filter kept, before the limit. */
  readonly matched: number
  readonly limit: number
  /** Chip counts over the scope, the capture filter and the excludes, never over the includes. */
  readonly facets: Facets.Facets
  readonly rows: readonly QueueRow[]
}

/**
 * One replay of one call, as the run index LISTS it: how it ENDED, in the four
 * fields a chip and a picker need. A capture gains a cell per sweep and the
 * page reads one of them, so everything else — the verdict itself, the
 * document, the recording, the delta sheet — is fetched per cell (`cell()`).
 */
export interface StudyCell {
  /** The cell directory name: `<case>__<shape>__<lane>`. */
  readonly name: string
  readonly dir: string
  /** The lane this cell ran on, as its directory name states it. */
  readonly lane?: string
  /** When this call's evidence was last written: the mtime of its cell dir. */
  readonly writtenAt?: string
  /** `verdict.json`'s own status word; absent where the cell wrote no verdict. */
  readonly status?: string
  /**
   * Whether the cell passed, as the run's index (`campaign.json`) states it:
   * the one verdict the store records too, the RFC audit and the bless
   * included. Absent where the run wrote no index naming the cell.
   */
  readonly passed?: boolean
  /** The run's index says the cell carries a source fault: information beside `passed`. */
  readonly sourceFault?: boolean
  /** The step the verdict says the replay died at. */
  readonly failedStep?: string
  /**
   * The lane verdict this cell was SKIPPED on, where it was. A skipped cell
   * never ran, so it carries no verdict, no recording and no document of its
   * own — it is listed so the run states what it declined to ask.
   */
  readonly skipped?: string
  /**
   * The interpreter refused this case: it holds a refusal instead of a bundle.
   * A refused case is a FAILING case with no evidence of its own, and dropping
   * it would hide the very case the run failed on; the refusal text itself is
   * evidence, so it travels with the cell.
   */
  readonly refused: boolean
}

/** A cell's verification lines and the final status they give it. */
export interface CellVerifications {
  readonly status: Verification.FinalStatus
  readonly lines: ReadonlyArray<VerificationLineView>
}

/** The evidence of ONE cell: everything the run index deliberately leaves behind. */
export interface StudyCellDetail {
  readonly name: string
  readonly dir: string
  readonly legs: readonly string[]
  readonly verdict: unknown
  readonly classification: unknown
  /**
   * The post-run RFC audit (`rfc.json`): what the traffic broke, gating or not.
   * It is not the verdict — a cell whose verdict passed is still red when the
   * audit gates — so it travels beside it rather than folded into it.
   */
  readonly rfc: unknown
  /** The lane verdict this cell was skipped on, as the file states it. */
  readonly skipped: unknown
  /** The interpreter's refusal, verbatim, where the cell holds one. */
  readonly error?: string
  /** What the interpreter wrote on stderr, where the cell ran and did not pass (`stderr.txt`). */
  readonly stderr?: string
  readonly pivot: unknown
  readonly timing: unknown
  readonly runConfig: unknown
  readonly runSpec: unknown
  /** Verbatim `recording/<leg>.jsonl` contents, keyed by leg. */
  readonly recording: Readonly<Record<string, string>>
  /**
   * The exchanges this cell's SUT had with the routing service, read off
   * `http.json`: what it asked and what it was served, in wire order. `null`
   * where the lane recorded none.
   */
  readonly http: HttpExchanges | null
  /**
   * The calls the SUT reported terminated (`call.terminated.json`, which a
   * failing cell keeps gzipped and a passing one drops); null where none is kept.
   */
  readonly terminated: unknown
  /** Decoded `confrontation.ndjson` rows; [] where the bundle has none. */
  readonly confrontation: readonly unknown[]
  /** Why the confrontation output did not decode, when it did not. */
  readonly confrontationProblem?: string
  /** The Call-IDs this cell minted and the window it drove the SUT in; null where it states none. */
  readonly join: SutEvidence.CellJoin | null
  /** The cell's `sut-log` attachments: the SUT's log over its window; [] where it has none. */
  readonly sutLogs: readonly SutEvidence.SutLog[]
  /** The contexts the SUT still held after a batch whose call this cell placed. */
  readonly held: readonly unknown[]
  /**
   * The cell's verification lines as the store holds them under its run
   * (`run<id>`) and its verdict's case, and the final status they give it;
   * null where the store holds none (a cell that did not run, a run not ingested).
   */
  readonly verifications: CellVerifications | null
}

export interface StudySession {
  /** The run directory name (`run1`, `run2`, …). */
  readonly name: string
  readonly dir: string
  /** Set where the run is set aside (`run-dirs.ts`): what made it. No default lands on it. */
  readonly aside?: string
  /** Set where the run was forced onto a lane lacking these capabilities (`runs.forced`). */
  readonly forced?: ReadonlyArray<string>
  /** When the run directory was last written: what says how fresh a run is. */
  readonly writtenAt?: string
  /** The output rollup the driver wrote at the run root. */
  readonly campaign: unknown
  readonly cells: readonly StudyCell[]
  /** What each batch's gate read of the SUT's call contexts (`contexts.ndjson`); [] where none did. */
  readonly held: readonly unknown[]
}

export interface StudyCase {
  readonly id: string
  /** Where this case materialises: the content cache entry a run cites. */
  readonly dir: string
  /** The standing cut's v3 document; null for a corpus-only case. */
  readonly document: unknown
  /**
   * The case's ledger row — the answers the backend mock serves on its call
   * and failure routes — as the viewer's mock answers (`ledger-view.ts`). `null`
   * where the case dir holds none, which is every refused case.
   */
  readonly ledger: MockAnswers | null
}

/**
 * The reading the cases and the flows document come from: which prep cut this
 * capture, when, and what it broke on. Two lanes pinned to different preps is a
 * real state, so what a run replayed and what stands cut now are stated apart.
 */
export interface StudyCut {
  readonly prepId: number
  readonly cutAt: string
  /** The cut failed with this reason; null for a cut that produced a reading. */
  readonly broke: string | null
  readonly cases: number
}

/** The queue's own row for this capture, so the study and the queue agree. */
export interface StudyStanding {
  readonly lane: string
  readonly runId: number
  /** The prep the standing run read its cases as of; null for a run pinned to none. */
  readonly prepId: number | null
  readonly bundlePath: string
  readonly cases: number
  readonly failed: number
  /** Steps the failing cases died at: the defect the study opens on. */
  readonly failedSteps: readonly string[]
  /** Every case of the run, as itself: which call, and how it ended. */
  readonly caseResults: ReadonlyArray<Model.CaseOutcome>
  /** Findings of the run's post-run RFC audit that fail its cells: the SUT's gating ones (`rfc.json` in each cell). */
  readonly rfcGating: number
  readonly finishedAt: string | null
  readonly lastOkRunId: number | null
  readonly lastOkBundlePath: string | null
  /** Each case's current run: its latest `@develop` run holding it, of any set kind. */
  readonly caseRuns: ReadonlyArray<Model.CaseRun>
}

export interface TriageStudy {
  readonly capture: string
  /** The one call this study is of; absent for the capture whole. */
  readonly case?: string
  /** The standing cut's refusal of that call, where it is one the cut made no case of. */
  readonly refusal?: Model.Refusal
  readonly disposition: Triage.Disposition
  readonly record: unknown
  readonly ticket?: TicketView
  /** What the run database says this capture currently stands at on the queue's lane. */
  readonly standing?: StudyStanding
  /** What the cut store says this capture stands cut as; absent where no cut does. */
  readonly cut?: StudyCut
  readonly corpusDir?: string
  /**
   * The flows document — the standing cut's enriched copy, else the corpus one
   * — WHERE it is and how many legs it holds. Not one leg of it: a response is
   * one JSON string, and a capture of thousands of calls has a document V8
   * refuses to hold as one. The legs come per window, off their own routes.
   */
  readonly flows?: { readonly path: string; readonly legs: number }
  /**
   * The line the capture's failed extraction left, as the deployment's
   * `Extraction` reads it; absent where it left none, where no extraction is
   * composed, and in the study of one call.
   */
  readonly extractionFailure?: string
  /** The generator's refusal to cut, where it wrote one: the `no-cases` evidence. */
  readonly exclusions?: { readonly path: string; readonly document: unknown }
  readonly cases: readonly StudyCase[]
  readonly sessionDir?: string
  readonly sessions: readonly StudySession[]
  /**
   * The repository the paths above are absolute inside. A registry record cites
   * its evidence REPO-RELATIVE, so a page that offers a record for copying has
   * to be able to turn a bundle dir back into the form the file holds.
   */
  readonly repoRoot: string
}

/** A capture's flows document with its legs left out, and how many it holds. */
export interface StudyFlowsEnvelope {
  readonly path: string
  readonly legs: number
  readonly document: unknown
}

/** A window of a flows document's legs, at the indices the document gives them. */
export interface StudyFlowsLegs {
  readonly from: number
  readonly legs: readonly unknown[]
}

export interface Interface {
  readonly queue: (request: QueueRequest) => Effect.Effect<TriageQueue>
  /** One capture on one lane — or one call of it; the lane defaults to the configured one. */
  readonly study: (capture: string, lane?: string, caseId?: string) => Effect.Effect<TriageStudy, NotFound>
  /** The capture's flows document with no leg in it; a capture with no document is a 404. */
  readonly flowsEnvelope: (capture: string) => Effect.Effect<StudyFlowsEnvelope, NotFound>
  /** `count` legs of it from `from`, in document order; `count` is defaulted and capped here. */
  readonly flowsLegs: (
    capture: string,
    from: number,
    count?: number
  ) => Effect.Effect<StudyFlowsLegs, NotFound>
  /** One cell's evidence, addressed as the study lists it: run name, cell name. */
  readonly cell: (
    capture: string,
    session: string,
    cell: string
  ) => Effect.Effect<StudyCellDetail, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Triage") {}

const FLOWS_SUFFIX = ".flows.json"

/** How many stored documents are remembered by hash: an entry is a count or a path. */
const STORED_FLOWS = 256

/** How a stored document is served, once the store has been asked. */
type StoredFlows = { readonly kind: "rows"; readonly legs: number } | {
  readonly kind: "file"
  readonly file: string
}


/**
 * The lane a cell ran on, off its directory name (`<case>__<shape>__<lane>`) —
 * the driver's naming contract, and what says which SUT a bundle answers for.
 */
const laneOfCell = (name: string): string | undefined => {
  const parts = name.split("__")
  return parts.length < 3 ? undefined : parts[parts.length - 1]
}

/** A cell's verdict as the run's index states it. */
interface Indexed {
  readonly passed: boolean
  /** It carries a source fault: information beside `passed`. */
  readonly sourceFault: boolean
}

/**
 * Cell directory → its verdict, as a run's `campaign.json` names its cells;
 * empty where the run wrote no index or one that names none.
 */
const indexedVerdicts = (campaign: unknown): ReadonlyMap<string, Indexed> => {
  const cells = typeof campaign === "object" && campaign !== null
    ? (campaign as Record<string, unknown>)["cells"]
    : undefined
  if (!Array.isArray(cells)) return new Map()
  return new Map(cells.flatMap((cell: unknown) => {
    const one = typeof cell === "object" && cell !== null ? cell as Record<string, unknown> : {}
    return typeof one["dir"] === "string"
      ? [[one["dir"], { passed: one["passed"] === true, sourceFault: one["sourceFault"] === true }] as const]
      : []
  }))
}

/** The session workdir stem: the capture basename with its pcap suffix off. */
export const captureStem = (capture: string): string => {
  for (const suffix of [".pcapng.gz", ".pcap.gz", ".pcapng", ".pcap"]) {
    if (capture.endsWith(suffix)) return capture.slice(0, -suffix.length)
  }
  return capture
}

/** The scope a request names, where it is one the queue knows; the default otherwise. */
export const scopeOf = (requested: string | undefined): Facets.Scope =>
  (Facets.SCOPES as ReadonlyArray<string>).includes(requested ?? "")
    ? requested as Facets.Scope
    : Facets.DEFAULT_SCOPE

/** The store's query for a request's filter words: what every reader of the queue's slice asks. */
export const queryOf = (request: QueueRequest): Facets.Query => ({
  scope: scopeOf(request.scope),
  capture: request.q?.trim(),
  rule: request.rule,
  expected: request.expected,
  refusal: request.refusal,
  outcome: request.outcome,
  feature: request.feature,
  source: request.source,
  exclude: {
    rule: request["not-rule"],
    expected: request["not-expected"],
    refusal: request["not-refusal"],
    outcome: request["not-outcome"],
    feature: request["not-feature"],
    source: request["not-source"]
  }
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service
    const runs = yield* Runs.Service
    const verifications = yield* Verifications.Service
    const cuts = yield* Cuts.Service
    const calls = yield* Calls.Service
    const refusals = yield* Refusals.Service
    const tickets = yield* TicketStates.Service
    const extraction = yield* Extraction.Service
    const pack = yield* SitePack.Service
    const sutLogs = pack.sutLogs
    const triageForms = pack.triage.forms
    const walks = FlowsWalk.make()
    const stored = makeMemo<StoredFlows>(STORED_FLOWS)

    // A missing or unreadable file serves as null, never undefined: the study
    // payload crosses the wire under `Schema.Unknown`, which is a JSON value,
    // and undefined is not one.
    const readJson = Effect.fn("Triage.readJson")(function*(file: string) {
      const text = yield* fs.readFileString(file).pipe(Effect.orElseSucceed(() => ""))
      if (text === "") return null
      return yield* Effect.try({ try: () => JSON.parse(text) as unknown, catch: () => null }).pipe(
        Effect.orElseSucceed(() => null)
      )
    })

    const readText = (file: string) =>
      fs.readFileString(file).pipe(Effect.orElseSucceed(() => undefined))

    /**
     * The cell's terminated calls: `call.terminated.json` where the interpreter
     * left it whole, else its gzipped copy; null where neither reads.
     */
    const readTerminated = Effect.fn("Triage.readTerminated")(function*(dir: string) {
      const whole = yield* readJson(path.join(dir, TERMINATED))
      if (whole !== null) return whole
      const bytes = yield* fs.readFile(path.join(dir, `${TERMINATED}.gz`)).pipe(Effect.orElseSucceed(() => null))
      if (bytes === null) return null
      return yield* Effect.try({
        try: () => JSON.parse(zlib.gunzipSync(bytes).toString("utf8")) as unknown,
        catch: () => null
      }).pipe(Effect.orElseSucceed(() => null))
    })

    /** A bundle's `http.json` as the viewer's exchanges; null where the bundle holds none. */
    const readHttp = (file: string) =>
      Effect.map(readJson(file), (document) => document === null ? null : parseHttpExchanges(document))

    const listDir = (dir: string) =>
      fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as string[]))

    /**
     * The case's ledger row: the answers the mock serves for this case, as the
     * cut wrote them. One row per case dir, named by the ledger number, so the
     * first `.json` under `ledger/` IS it.
     */
    const readLedgerRow = Effect.fn("Triage.readLedgerRow")(function*(dir: string) {
      const ledgerDir = path.join(dir, "ledger")
      const row = (yield* listDir(ledgerDir)).sort().find((entry) => entry.endsWith(".json"))
      return row === undefined ? null : mockAnswersOf(yield* readJson(path.join(ledgerDir, row)))
    })

    /** A directory's mtime as an ISO instant: when its run last wrote. */
    const writtenAt = (dir: string) =>
      fs.stat(dir).pipe(
        Effect.map((info) => Option.getOrUndefined(info.mtime)?.toISOString()),
        Effect.orElseSucceed(() => undefined)
      )

    const isDir = (candidate: string) =>
      fs.stat(candidate).pipe(
        Effect.map((info) => info.type === "Directory"),
        Effect.orElseSucceed(() => false)
      )

    /** The registry, decoded STRICT; an unreadable registry serves as empty, loudly in `detail`. */
    const readRegistry = Effect.fn("Triage.readRegistry")(function*(registryPath: string) {
      const text = yield* readText(registryPath)
      if (text === undefined) return { registry: {} as Triage.Registry, problem: `${registryPath} is not readable` }
      return yield* Effect.try({
        try: () => ({ registry: Triage.decodeRegistrySync(JSON.parse(text) as unknown), problem: undefined }),
        catch: (cause) => ({ registry: {} as Triage.Registry, problem: `${registryPath}: ${String(cause)}` })
      }).pipe(Effect.orElseSucceed(() => ({
        registry: {} as Triage.Registry,
        problem: `${registryPath} did not decode`
      })))
    })

    const ticketOf = Effect.fn("Triage.ticketOf")(function*(record: Triage.RegistryRecord) {
      if (Triage.isFinal(record)) return undefined
      const states = yield* tickets.states([record.ticket])
      return {
        ref: record.ticket,
        state: states.get(record.ticket) ?? "missing",
        ...(tickets.unavailable === undefined ? {} : { unavailable: tickets.unavailable })
      } satisfies TicketView
    })

    /**
     * The generator's `excluded.json` for a capture: what the latest cut
     * refused, in its session workdir. Absent = the capture was never refused.
     */
    const locateExclusions = Effect.fn("Triage.locateExclusions")(function*(
      triage: ViewerConfig.TriagePaths,
      capture: string
    ) {
      const file = excludedFile(triage.sessions, capture)
      const document = yield* readJson(file)
      return document === null ? undefined : { path: file, document }
    })

    const withBundle = Effect.fn("Triage.withBundle")(function*(row: Calls.CallDetail) {
      return {
        ...row,
        bundle: row.bundlePath === null ? false : yield* isDir(row.bundlePath)
      } satisfies QueueRow
    })

    const queue = Effect.fn("Triage.queue")(function*(request: QueueRequest) {
      const triage = config.triage
      const scope = scopeOf(request.scope)
      const limit = request.limit ?? QUEUE_LIMIT
      const nothing = {
        rules: [],
        failures: [],
        expected: [],
        refusals: [],
        outcomes: [],
        features: [],
        sources: [],
        captures: 0
      } satisfies Facets.Facets
      if (triage === undefined) {
        return {
          available: false,
          detail: `no "triage" block in ${config.configPath}`,
          lanes: [],
          scope,
          matched: 0,
          limit,
          facets: nothing,
          rows: []
        } satisfies TriageQueue
      }
      const lanes = yield* runs.lanes.pipe(Effect.orElseSucceed(() => [] as ReadonlyArray<Runs.Lane>))
      // A lane nothing has ever run on is still a legitimate question — it
      // answers "nothing here", which is what the reader asked.
      const lane = request.lane ?? triage.lane
      const query = queryOf(request)
      const empty: Calls.Queue = { rows: [], matched: 0, captures: 0, groups: [] }
      const listed = yield* calls.queue(lane, query, limit).pipe(
        Effect.map((queue) => ({ queue, problem: undefined as string | undefined })),
        Effect.catch((cause) => Effect.succeed({ queue: empty, problem: String(cause) }))
      )
      const served = listed.queue.rows
      // A store that lists but will not hydrate still serves the page: the
      // rows say what they are, their documents' facts read as unknown, and
      // `detail` says why.
      const hydrated = yield* calls.hydrate(served).pipe(
        Effect.map((rows) => ({ rows, problem: undefined as string | undefined })),
        Effect.catch((cause) =>
          Effect.succeed({
            rows: served.map((row): Calls.CallDetail => ({ ...row, ...CallSummary.EMPTY })),
            problem: String(cause)
          })
        )
      )
      return {
        available: true,
        // A filter keeping nothing is an answer; a lane no run was made on is the
        // reason nothing can be served.
        detail: listed.problem ?? hydrated.problem ??
          (listed.queue.matched === 0 && !lanes.some((known) => known.lane === lane)
            ? `the run database holds no call on lane ${lane}`
            : ""),
        lane,
        lanes,
        scope,
        matched: listed.queue.matched,
        limit,
        facets: { ...Facets.facets(listed.queue.groups, query), captures: listed.queue.captures },
        rows: yield* Effect.forEach(hydrated.rows, withBundle)
      } satisfies TriageQueue
    })

    /** A string field of a JSON object, where the object is one and it holds one. */
    const field = (document: unknown, key: string): string | undefined => {
      const value = typeof document === "object" && document !== null
        ? (document as Record<string, unknown>)[key]
        : undefined
      return typeof value === "string" ? value : undefined
    }

    /**
     * How a cell ENDED, and nothing more: the picker and the chips read this,
     * and a capture swept hundreds of times would otherwise ship hundreds of
     * verdicts to state two of them.
     */
    const readCell = Effect.fn("Triage.readCell")(function*(dir: string, name: string, indexed: Indexed | undefined) {
      const cellWrittenAt = yield* writtenAt(dir)
      const cellLane = laneOfCell(name)
      const verdict = yield* readJson(path.join(dir, "verdict.json"))
      const skipped = field(yield* readJson(path.join(dir, "skipped.json")), "verdict")
      const status = field(verdict, "status")
      const failedStep = field(verdict, "failed_step")
      return {
        name,
        dir,
        ...(cellLane === undefined ? {} : { lane: cellLane }),
        ...(cellWrittenAt === undefined ? {} : { writtenAt: cellWrittenAt }),
        ...(status === undefined ? {} : { status }),
        ...(indexed === undefined ? {} : { passed: indexed.passed }),
        ...(indexed?.sourceFault === true ? { sourceFault: true } : {}),
        ...(failedStep === undefined ? {} : { failedStep }),
        ...(skipped === undefined ? {} : { skipped }),
        refused: yield* fs.exists(path.join(dir, "error.txt")).pipe(Effect.orElseSucceed(() => false))
      } satisfies StudyCell
    })

    /** A run bundle plus the post-run outputs the case/run catalog does not read. */
    const readCellDetail = Effect.fn("Triage.readCellDetail")(function*(dir: string, name: string) {
      const { recording, pivot } = yield* CallEvidence.readCellRecording(dir).pipe(Effect.provideService(FileSystem, fs))
      const confrontationText = yield* readText(path.join(dir, "confrontation.ndjson"))
      const confrontation = confrontationText === undefined
        ? { rows: [] as readonly unknown[], problem: undefined }
        : yield* Effect.try({
          try: () => ({
            rows: Confrontation.parseConfrontationLines(confrontationText) as readonly unknown[],
            problem: undefined
          }),
          catch: (cause) => ({ rows: [] as readonly unknown[], problem: String(cause) })
        }).pipe(Effect.orElseSucceed(() => ({
          rows: [] as readonly unknown[],
          problem: "confrontation.ndjson did not decode"
        })))
      const error = yield* readText(path.join(dir, "error.txt"))
      const stderr = yield* readText(path.join(dir, "stderr.txt"))
      const sut = yield* SutEvidence.ofCell(dir, sutLogs).pipe(Effect.provideService(FileSystem, fs), Effect.provideService(Path, path))
      return {
        name,
        dir,
        legs: Object.keys(recording),
        verdict: yield* readJson(path.join(dir, "verdict.json")),
        classification: yield* readJson(path.join(dir, "classification.json")),
        rfc: yield* readJson(path.join(dir, "rfc.json")),
        skipped: yield* readJson(path.join(dir, "skipped.json")),
        ...(error === undefined ? {} : { error }),
        ...(stderr === undefined ? {} : { stderr }),
        pivot,
        timing: yield* readJson(path.join(dir, "timing.json")),
        runConfig: yield* readJson(path.join(dir, "run-config.json")),
        runSpec: (yield* readJson(path.join(dir, "run-spec.json"))) ??
          (yield* readJson(path.join(path.dirname(dir), "specs", `${name}.run-spec.json`))),
        recording,
        http: yield* readHttp(path.join(dir, "http.json")),
        terminated: yield* readTerminated(dir),
        confrontation: confrontation.rows,
        ...(confrontation.problem === undefined ? {} : { confrontationProblem: confrontation.problem }),
        join: sut.join,
        sutLogs: sut.sutLogs,
        held: sut.held
      }
    })

    /** The lines the store holds of the cell whose verdict is `verdict` in the run named `session`; null where it holds none. */
    const verificationsOf = (session: string, verdict: unknown) =>
      Effect.gen(function*() {
        const runId = RunDirs.runIdOf(session)
        const caseId = typeof verdict === "object" && verdict !== null ? (verdict as { readonly case?: unknown }).case : undefined
        if (runId === undefined || typeof caseId !== "string") return null
        const lines = yield* verifications.of({ scope: "cell", runId, caseId }).pipe(Effect.orDie)
        return lines.length === 0 ? null : { status: Verification.finalStatusOf(lines), lines: lines.map(lineViewOf) }
      })

    /** The cells of one run — those of one case where `caseId` names it — and what ran around them. */
    const readSession = Effect.fn("Triage.readSession")(function*(
      dir: string,
      name: string,
      caseId?: string
    ) {
      const campaign = yield* readJson(path.join(dir, "campaign.json"))
      const indexOf = indexedVerdicts(campaign)
      const cells: StudyCell[] = []
      for (const entry of (yield* listDir(dir)).sort()) {
        if (caseId !== undefined && !entry.startsWith(`${caseId}__`)) continue
        const cellDir = path.join(dir, entry)
        if (!(yield* isDir(cellDir))) continue
        const isBundle = (yield* fs.exists(path.join(cellDir, "pivot.json")).pipe(
          Effect.orElseSucceed(() => false)
        )) ||
          (yield* fs.exists(path.join(cellDir, "recording")).pipe(Effect.orElseSucceed(() => false))) ||
          (yield* fs.exists(path.join(cellDir, "skipped.json")).pipe(Effect.orElseSucceed(() => false))) ||
          // A refusal is a cell: the case the run failed on may hold nothing else.
          (yield* fs.exists(path.join(cellDir, "error.txt")).pipe(Effect.orElseSucceed(() => false)))
        if (isBundle) cells.push(yield* readCell(cellDir, entry, indexOf.get(entry)))
      }
      const runWrittenAt = yield* writtenAt(dir)
      return {
        name,
        dir,
        ...(runWrittenAt === undefined ? {} : { writtenAt: runWrittenAt }),
        campaign: caseId === undefined ? campaign : null,
        cells,
        held: yield* SutEvidence.ofRun(dir).pipe(Effect.provideService(FileSystem, fs), Effect.provideService(Path, path))
      } satisfies StudySession
    })

    /** What the standing cut says of a capture; null where no cut stands on it. */
    const standingCut = Effect.fn("Triage.standingCut")(function*(capture: string) {
      return yield* cuts.standing(capture).pipe(
        Effect.orElseSucceed(() => null as Model.CaptureCut | null)
      )
    })

    /**
     * How the document a hash names is served: its legs' rows, or — for a
     * document a cut kept whole in the envelope's head, which is every document
     * the live store holds — the content cache file that head is written to. The
     * hash names the content, so the answer is remembered and the head comes out
     * of the store once; a cache file since deleted is written again, never
     * answered as a capture with no document.
     */
    const storedFlows = Effect.fn("Triage.storedFlows")(function*(sessions: string, hash: string) {
      const held = stored.get(hash)
      const stands = held !== undefined &&
        (held.kind === "rows" || (yield* fs.exists(held.file).pipe(Effect.orElseSucceed(() => false))))
      if (held !== undefined && stands) return held
      const envelope = yield* cuts.flowsEnvelope(hash).pipe(Effect.orElseSucceed(() => null))
      if (envelope === null) return null
      if (envelope.legs > 0 || envelope.tail.length > 0) {
        const paged = { kind: "rows", legs: envelope.legs } as const
        stored.put(hash, paged)
        return paged
      }
      // A cache that cannot be written is a capture with no document served,
      // never a document held whole to serve it.
      const file = yield* Content.materialiseFlowsFrom(sessions, hash, envelope).pipe(
        Effect.provideService(Cuts.Service, cuts),
        Effect.provideService(FileSystem, fs),
        Effect.catchCause(() => Effect.succeed(null))
      )
      if (file === null) return null
      const written = { kind: "file", file } as const
      stored.put(hash, written)
      return written
    })

    /**
     * Where a capture's flows document is read from: the standing cut's leg
     * rows, else a FILE — the corpus extraction's own document for a capture no
     * cut stands on, or the content cache for a document held whole in one row.
     * `null` for a capture neither holds a document of.
     */
    const flowsSource = Effect.fn("Triage.flowsSource")(function*(
      capture: string,
      standing?: Model.CaptureCut | null
    ) {
      const triage = config.triage
      if (triage === undefined || !areSegments(capture)) return null
      const cut = standing === undefined ? yield* standingCut(capture) : standing
      const hash = cut?.flowsHash ?? null
      if (hash !== null) {
        const held = yield* storedFlows(triage.sessions, hash)
        // The store's own document, whichever shape it is in, is the answer: a
        // capture the store holds a cut of never falls back to the corpus's
        // other, un-enriched document.
        if (held !== null && held.kind === "rows") {
          return {
            kind: "rows",
            hash,
            legs: held.legs,
            path: Content.flowsFile(triage.sessions, hash)
          } as const
        }
        if (held !== null) {
          const walked = yield* Effect.sync(() => walks.walk(held.file))
          return walked === null ? null : { kind: "file", path: held.file, walked } as const
        }
      }
      const dir = path.join(triage.corpus, capture)
      for (const entry of (yield* listDir(dir)).sort()) {
        if (!entry.endsWith(FLOWS_SUFFIX)) continue
        const file = path.join(dir, entry)
        const walked = yield* Effect.sync(() => walks.walk(file))
        if (walked !== null) return { kind: "file", path: file, walked } as const
      }
      return null
    })

    /**
     * Where a capture's flows document is and how many legs it holds. Neither
     * source is read for it: the store counts its rows, and a file states its
     * count out of the walk the window will be read by.
     */
    const flowsPlace = Effect.fn("Triage.flowsPlace")(function*(
      capture: string,
      standing: Model.CaptureCut | null
    ) {
      const source = yield* flowsSource(capture, standing)
      if (source === null) return undefined
      return {
        path: source.path,
        legs: source.kind === "rows" ? source.legs : source.walked.legs
      }
    })

    /**
     * The document with its legs left out: the envelope's two halves meet
     * around an empty `legs` array, so nothing here is as wide as the legs. A
     * stored envelope that does not parse is a defect of the store, never an
     * answer.
     */
    const flowsEnvelope = Effect.fn("Triage.flowsEnvelope")(function*(capture: string) {
      const source = yield* flowsSource(capture)
      if (source === null) return yield* new NotFound({ id: `${capture}/flows` })
      if (source.kind === "file") {
        return {
          path: source.path,
          legs: source.walked.legs,
          document: yield* Effect.try({
            try: () => source.walked.envelope(),
            catch: (error) => error as Error
          }).pipe(Effect.orDie)
        } satisfies StudyFlowsEnvelope
      }
      const envelope = yield* cuts.flowsEnvelope(source.hash).pipe(Effect.orElseSucceed(() => null))
      if (envelope === null) return yield* new NotFound({ id: `${capture}/flows` })
      const document = yield* Effect.try({
        try: () => CallEvidence.envelopeDocument(envelope),
        catch: (error) => error as Error
      }).pipe(Effect.orDie)
      return { path: source.path, legs: envelope.legs, document } satisfies StudyFlowsEnvelope
    })

    /**
     * One window of the document's legs, read from the rows the store keeps
     * them in or at the places the file walk found them. `servedWindow` caps it
     * and holds it inside the document: the store indexes a leg in an int4
     * column, so a window outside the document is an empty window at its end,
     * never a failed query.
     */
    const flowsLegs = Effect.fn("Triage.flowsLegs")(function*(
      capture: string,
      from: number,
      count?: number
    ) {
      const source = yield* flowsSource(capture)
      if (source === null) return yield* new NotFound({ id: `${capture}/flows` })
      const held = source.kind === "rows" ? source.legs : source.walked.legs
      const { count: limit, from: start } = servedWindow(held, from, count)
      if (source.kind === "file") {
        return {
          from: start,
          legs: yield* Effect.try({
            try: () => source.walked.window(start, limit),
            catch: (error) => error as Error
          }).pipe(Effect.orDie)
        } satisfies StudyFlowsLegs
      }
      if (limit === 0) return { from: start, legs: [] } satisfies StudyFlowsLegs
      const rows = yield* cuts.flowsLegs(source.hash, start, limit).pipe(Effect.orDie)
      const legs = yield* Effect.try({
        try: () => CallEvidence.legDocuments(rows),
        catch: (error) => error as Error
      }).pipe(Effect.orDie)
      return { from: start, legs } satisfies StudyFlowsLegs
    })

    /**
     * The cases the standing cut made of this capture — the one `caseId` names,
     * where it names one, with nothing of the others read: `cut_cases` names
     * them and `case_documents` holds what each replays. A cut that refused
     * every call is an answer — no case — and only a capture the store holds no
     * cut of at all falls back to the corpus, which states ids without documents.
     */
    const studyCases = Effect.fn("Triage.studyCases")(function*(
      triage: ViewerConfig.TriagePaths,
      capture: string,
      cut: Model.CaptureCut | null,
      caseId?: string
    ) {
      const wanted = (id: string): boolean => caseId === undefined || id === caseId
      if (cut !== null) {
        const fromCut: StudyCase[] = []
        for (const entry of cut.cases.filter((one) => wanted(one.caseId))) {
          const content = yield* cuts.content(entry.caseHash).pipe(
            Effect.orElseSucceed(() => null as Model.CaseContent | null)
          )
          const dir = path.join(Content.caseDir(triage.sessions, entry.caseHash), entry.caseId)
          fromCut.push({
            id: entry.caseId,
            dir,
            document: content?.document ?? null,
            ledger: yield* readLedgerRow(dir)
          })
        }
        return fromCut
      }
      const corpusCases = path.join(triage.corpus, capture, "cases")
      const fromCorpus: StudyCase[] = []
      for (const entry of (yield* listDir(corpusCases)).sort().filter(wanted)) {
        const dir = path.join(corpusCases, entry)
        fromCorpus.push({ id: entry, dir, document: null, ledger: yield* readLedgerRow(dir) })
      }
      return fromCorpus
    })

    /** The generator's records of one call, out of the capture's `excluded.json`; undefined where it holds none. */
    const exclusionsOf = (
      exclusions: { readonly path: string; readonly document: unknown } | undefined,
      caseId: string
    ) => {
      if (exclusions === undefined || !Array.isArray(exclusions.document)) return undefined
      const records = exclusions.document.filter((entry) =>
        typeof entry === "object" && entry !== null && (entry as Record<string, unknown>)["caseId"] === caseId
      )
      return records.length === 0 ? undefined : { path: exclusions.path, document: records }
    }

    const study = Effect.fn("Triage.study")(function*(capture: string, lane?: string, caseId?: string) {
      const triage = config.triage
      // The capture names a directory under three roots below: a name that is
      // not a segment would read outside all three (`./segment.ts`).
      if (triage === undefined || !areSegments(capture)) return yield* new NotFound({ id: capture, accepted: triageForms.capture })
      const corpusDir = path.join(triage.corpus, capture)
      const sessionDir = path.join(triage.sessions, captureStem(capture))
      const { registry } = yield* readRegistry(triage.registry)
      const record = registry[capture]
      const hasCorpus = yield* isDir(corpusDir)
      const hasSession = yield* isDir(sessionDir)
      const standing = (yield* runs.standing(capture, lane ?? triage.lane).pipe(
        Effect.orElseSucceed(() => null as Model.Standing | null)
      )) ?? undefined
      const cut = yield* standingCut(capture)
      if (!hasCorpus && !hasSession && cut === null && record === undefined) {
        return yield* new NotFound({ id: capture, accepted: triageForms.capture })
      }

      const cases = yield* studyCases(triage, capture, cut, caseId)
      const refusal = caseId === undefined || cases.length > 0
        ? undefined
        : (yield* refusals.ofCase(capture, caseId).pipe(Effect.orElseSucceed(() => null))) ?? undefined
      const ofCall = <A extends { readonly caseId: string }>(entries: ReadonlyArray<A>): ReadonlyArray<A> =>
        caseId === undefined ? entries : entries.filter((entry) => entry.caseId === caseId)

      const sessions: StudySession[] = []
      if (hasSession) {
        // Numeric order (run10 after run9), the runs set aside after the rest.
        const dirs = yield* RunDirs.runDirsOf(sessionDir).pipe(
          Effect.provideService(FileSystem, fs),
          Effect.provideService(Path, path)
        )
        // A run forced onto a lane lacking what it needs says so; a store that cannot answer forces none.
        const idOf = (name: string): number | undefined => {
          const digits = /^run(\d+)$/.exec(name)?.[1]
          return digits === undefined ? undefined : Number(digits)
        }
        const ids = dirs.flatMap((run) => idOf(run.name) ?? [])
        const forced = yield* runs.forced(ids).pipe(
          Effect.orElseSucceed(() => new Map<number, ReadonlyArray<string>>())
        )
        for (const run of dirs) {
          const session = yield* readSession(run.dir, run.name, caseId)
          const id = idOf(run.name)
          const lacks = id === undefined ? undefined : forced.get(id)
          const marked = lacks === undefined ? session : { ...session, forced: lacks }
          sessions.push(run.aside ? { ...marked, aside: RunDirs.ASIDE_LABEL } : marked)
        }
      }
      // A call nothing names — no case of the cut, no refusal, no cell of any
      // run — is not this capture's.
      if (
        caseId !== undefined && cases.length === 0 && refusal === undefined &&
        sessions.every((session) => session.cells.length === 0)
      ) {
        return yield* new NotFound({ id: `${capture}: no case ${caseId}`, accepted: triageForms.case })
      }
      const ticket = record === undefined ? undefined : yield* ticketOf(record)
      const flows = yield* flowsPlace(capture, cut)
      // The generator's records are the capture's whole; a cut case has no use for them.
      const exclusions = caseId === undefined
        ? yield* locateExclusions(triage, capture)
        : refusal === undefined
        ? undefined
        : exclusionsOf(yield* locateExclusions(triage, capture), caseId)
      const extractionFailure = caseId === undefined && hasCorpus
        ? yield* extraction.failureOf(corpusDir, capture).pipe(Effect.orElseSucceed(() => undefined))
        : undefined
      return {
        capture,
        ...(caseId === undefined ? {} : { case: caseId }),
        ...(refusal === undefined ? {} : { refusal }),
        disposition: Triage.disposition(record, ticket?.state),
        record: record ?? null,
        ...(ticket === undefined ? {} : { ticket }),
        ...(standing === undefined ? {} : {
          standing: {
            lane: standing.lane,
            runId: standing.runId,
            prepId: standing.prepId,
            bundlePath: standing.bundlePath,
            cases: standing.cases,
            failed: standing.failed,
            failedSteps: standing.failedSteps,
            caseResults: ofCall(standing.caseResults),
            rfcGating: standing.rfcGating,
            finishedAt: standing.finishedAt,
            lastOkRunId: standing.lastOkRunId,
            lastOkBundlePath: standing.lastOkBundlePath,
            caseRuns: ofCall(standing.caseRuns)
          }
        }),
        ...(cut === null ? {} : {
          cut: {
            prepId: cut.prepId,
            cutAt: cut.cutAt,
            broke: cut.broke,
            cases: cut.cases.length
          }
        }),
        ...(hasCorpus ? { corpusDir } : {}),
        ...(flows === undefined ? {} : { flows }),
        ...(extractionFailure === undefined ? {} : { extractionFailure }),
        ...(exclusions === undefined ? {} : { exclusions }),
        cases,
        ...(hasSession ? { sessionDir } : {}),
        sessions,
        repoRoot: triage.repoRoot
      } satisfies TriageStudy
    })

    /**
     * One cell's evidence. The capture, the run and the cell are NAMES the
     * study listed, never paths (`./segment.ts`), so a bundle outside this
     * capture's workdir cannot be addressed.
     */
    const cell = Effect.fn("Triage.cell")(function*(capture: string, session: string, name: string) {
      const triage = config.triage
      if (triage === undefined || !areSegments(capture, session, name)) {
        return yield* new NotFound({ id: `${capture}/${session}/${name}`, accepted: triageForms.cell })
      }
      const runDir = yield* RunDirs.runDirOf(path.join(triage.sessions, captureStem(capture)), session).pipe(
        Effect.provideService(FileSystem, fs),
        Effect.provideService(Path, path)
      )
      const dir = path.join(runDir, name)
      if (!(yield* isDir(dir))) return yield* new NotFound({ id: `${capture}/${session}/${name}`, accepted: triageForms.cell })
      const detail = yield* readCellDetail(dir, name)
      return { ...detail, verifications: yield* verificationsOf(session, detail.verdict) } satisfies StudyCellDetail
    })

    return Service.of({ queue, study, flowsEnvelope, flowsLegs, cell })
  })
)

export * as TriageCatalog from "./triage.ts"

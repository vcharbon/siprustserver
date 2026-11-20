/**
 * The compare and run reports the viewer serves: the very documents the
 * compare command and the run report build (`@sip/corpus` `report/compare-document.ts`), with
 * every link pointing at this viewer. A selector the store refuses, or that
 * names no run, is a {@link Refused} carrying the reason.
 *
 * The text a cited run is read by (`/api/triage/<capture>/runs/run<N>/text`)
 * falls back here once the run's bundle is gone: the run report of that run,
 * off its row, or off the summary a citation kept where the wipe dropped the
 * row — never a 404 for a run the store can still speak for.
 */
import * as CompareDocument from "@sip/corpus/report/compare-document"
import * as RunReportText from "@sip/corpus/report/run-report"
import type { CompareReport, RunReport } from "@sip/contracts"
import * as Resolver from "@sip/store/resolver"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { ViewerConfig } from "./config.ts"
import { SitePack } from "./site-pack.ts"

export class Refused extends Schema.TaggedError<Refused>()("Site.ReportRefused", {
  reason: Schema.String
}) {}

/** The run store did not answer the reading: its own message, never a bare server error. */
export class Unavailable extends Schema.TaggedError<Unavailable>()("Site.ReportUnavailable", {
  reason: Schema.String
}) {}

/** A store failure as the route states it. */
const unavailable = (cause: unknown): Unavailable =>
  new Unavailable({ reason: `the run store did not answer: ${cause instanceof Error ? cause.message : String(cause)}` })

export interface CompareQuery {
  /** The before side's selector; a query naming none is refused. */
  readonly a?: string | undefined
  /** The after side's selector; a query naming none is refused. */
  readonly b?: string | undefined
  /** The lane a side that pins none is read on; absent = the configured lane. */
  readonly lane?: string | undefined
  /** Every compared call, the unchanged ones too. */
  readonly all?: boolean | undefined
  /** Only the calls whose two ladders differ, whatever their class. */
  readonly ladderChanged?: boolean | undefined
  /** How many listed calls get their ladders read; absent = {@link LADDERS}. */
  readonly ladders?: number | undefined
}

export interface ReportQuery {
  /** The run set's selector; a query naming none is refused. */
  readonly run?: string | undefined
  readonly lane?: string | undefined
}

/** The ladders a page reads by default: as many as the compare command reads. */
export const LADDERS = 50

/** The runs a report lists, and the calls an only-before/only-after list names. */
const LISTED = 100

/** The calls a text fallback prints. */
const TEXT_CALLS = 200

export interface Interface {
  readonly compare: (query: CompareQuery) => Effect.Effect<CompareReport.CompareReport, Refused | Unavailable>
  readonly report: (query: ReportQuery) => Effect.Effect<RunReport.RunReport, Refused | Unavailable>
  /**
   * Run `runId` as text — over `caseId`'s call alone where it names one — off
   * the store's row or kept summary; where the store holds neither, what its
   * resolver says of the cited id (the forms it reads, or what the id is).
   */
  readonly runText: (
    capture: string,
    runId: number,
    lane: string | undefined,
    caseId: string | undefined
  ) => Effect.Effect<RunTextAnswer>
}

/** A run's text, or the resolver's note on why the store holds none of it. */
export type RunTextAnswer =
  | { readonly _tag: "text"; readonly text: string }
  | { readonly _tag: "missing"; readonly note: string }

export class Service extends Context.Service<Service, Interface>()("@sip/site/Reports") {}

/** The answer's document, or its refusal as the route's error. */
const documentOf = <A>(answer: CompareDocument.Answer<A>): Effect.Effect<A, Refused> =>
  answer._tag === "document" ? Effect.succeed(answer.document) : Effect.fail(new Refused({ reason: answer.reason }))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const store = yield* Effect.context<CompareDocument.Services | FileSystem | Path>()
    const resolver = yield* Resolver.Service
    const pack = yield* SitePack.Service
    const viewer = config.publicUrl
    const laneOf = (lane: string | undefined): string => lane ?? config.triage?.lane ?? pack.defaultLane

    const compare = Effect.fn("Reports.compare")(function*(query: CompareQuery) {
      const named = (side: string | undefined): side is string => side !== undefined && side.trim() !== ""
      if (!named(query.a) || !named(query.b)) {
        return yield* new Refused({
          reason: `name both sides: a=<selector>&b=<selector> (${!named(query.a) ? "a" : "b"} is missing)`
        })
      }
      const answer = yield* CompareDocument.compare({
        before: query.a,
        after: query.b,
        lane: laneOf(query.lane),
        all: query.all === true,
        ladderChanged: query.ladderChanged === true,
        ladders: query.ladders ?? LADDERS,
        only: LISTED,
        viewer
      }).pipe(Effect.provide(store), Effect.mapError(unavailable))
      return yield* documentOf<CompareReport.CompareReport>(answer)
    })

    const report = Effect.fn("Reports.report")(function*(query: ReportQuery) {
      if (query.run === undefined || query.run.trim() === "") {
        return yield* new Refused({ reason: "name the run set: run=<selector>" })
      }
      const answer = yield* CompareDocument.report({ selector: query.run, lane: laneOf(query.lane), viewer, runs: LISTED })
        .pipe(Effect.provide(store), Effect.mapError(unavailable))
      return yield* documentOf<RunReport.RunReport>(answer)
    })

    const runText = Effect.fn("Reports.runText")(function*(
      capture: string,
      runId: number,
      lane: string | undefined,
      caseId: string | undefined
    ) {
      const answer = yield* report({ run: `@run:${runId}`, lane }).pipe(
        Effect.map((doc): RunReport.RunReport | null => doc),
        Effect.catch(() => Effect.succeed(null))
      )
      const held = answer !== null && answer.runs.some((run) => run.capture === capture) &&
        (caseId === undefined || answer.calls.some((call) => call.caseId === caseId))
      if (answer === null || !held) {
        const cited = caseId === undefined ? `run${runId}` : `run${runId}/${caseId}`
        const said = yield* resolver.resolve(cited).pipe(
          Effect.map((resolution) => resolution.note),
          Effect.catch(() => Effect.succeed("the store did not answer"))
        )
        return { _tag: "missing", note: said } satisfies RunTextAnswer
      }
      const doc = caseId === undefined
        ? answer
        : { ...answer, calls: answer.calls.filter((call) => call.caseId === caseId) }
      const gone = answer.source === "runs"
        ? `this run's bundle is gone (retention); its row stands, and the report below is read off it`
        : `this run's bundle and row are gone; the report below is the summary a citation kept`
      return { _tag: "text", text: `${gone}\n\n${RunReportText.render(doc, TEXT_CALLS)}\n` } satisfies RunTextAnswer
    })

    return Service.of({ compare, report, runText })
  })
)

export * as Reports from "./reports.ts"

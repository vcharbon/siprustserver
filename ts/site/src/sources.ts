/**
 * The source list: `Sources.list` under the fingerprints this
 * tree's extractor and cut code come to — one statement, paged and counted
 * — and, per capture, the retire plan as the retire command prints
 * it before `--yes`: the rows, the runs and files on disk, and the running
 * jobs holding the capture. The page confirms on those numbers; the retire
 * itself is a job.
 */
import { Fingerprint } from "@sip/corpus"
import * as Retire from "@sip/corpus/ingest/retire"
import * as Source from "@sip/corpus/ingest/source"
import { Removal, Sources } from "@sip/store"
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { pathsOf } from "./paths.ts"
import { SitePack } from "./site-pack.ts"

export interface Query {
  readonly stale: boolean
  readonly q: string | undefined
  readonly limit: number
  readonly offset: number
}

export interface Page extends Sources.Page {
  readonly limit: number
  readonly offset: number
  /** The roots a path may be added from. */
  readonly roots: ReadonlyArray<string>
}

export interface RetirePlan {
  readonly capture: string
  readonly rows: Readonly<Record<string, number>>
  readonly runs: number
  readonly bundles: number
  readonly sessionDir: string | null
  readonly artifactDir: string | null
  readonly uploads: ReadonlyArray<string>
  readonly holders: ReadonlyArray<{ readonly jobId: number; readonly kind: string; readonly how: string }>
  readonly known: boolean
}

export interface Interface {
  readonly list: (query: Query) => Effect.Effect<Page>
  readonly retirePlan: (capture: string) => Effect.Effect<RetirePlan>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/SourceList") {}

/** The page cap: a listing of thousands is paged, never served whole. */
export const MAX_LIMIT = 500
export const DEFAULT_LIMIT = 100

/** A page with nothing in it: what a deployment with no triage block lists. */
const EMPTY: Page = { rows: [], total: 0, limit: DEFAULT_LIMIT, offset: 0, roots: [] }

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const sources = yield* Sources.Service
    const removal = yield* Removal.Service
    const host = yield* Effect.context<Host>()

    const list = Effect.fn("SourceList.list")(function*(query: Query) {
      const triage = config.triage
      if (triage === undefined) return EMPTY
      const paths = pathsOf(pack, triage)
      const limit = Math.min(MAX_LIMIT, Math.max(1, Math.floor(query.limit)))
      const offset = Math.max(0, Math.floor(query.offset))
      const page = yield* sources.list({
        ...(query.stale ? { stale: true } : {}),
        ...(query.q === undefined || query.q === "" ? {} : { q: query.q }),
        limit,
        offset,
        extractorHash: yield* Fingerprint.extractorNow.pipe(Effect.provide(host), Effect.orDie),
        cutFingerprint: yield* Fingerprint.cutNow.pipe(Effect.provide(host), Effect.orDie)
      }).pipe(Effect.orDie)
      return { ...page, limit, offset, roots: yield* Source.roots(paths).pipe(Effect.provide(host), Effect.orDie) } satisfies Page
    })

    const retirePlan = Effect.fn("SourceList.retirePlan")(function*(capture: string) {
      const triage = config.triage
      if (triage === undefined) {
        return {
          capture,
          rows: {},
          runs: 0,
          bundles: 0,
          sessionDir: null,
          artifactDir: null,
          uploads: [],
          holders: [],
          known: false
        } satisfies RetirePlan
      }
      const plans = yield* Retire.plan({ paths: pathsOf(pack, triage), captures: [capture], measure: false }).pipe(
        Effect.provideService(Removal.Service, removal),
        Effect.provide(host),
        Effect.orDie
      )
      const plan = plans[0]!
      return {
        capture,
        rows: { ...plan.rows },
        runs: plan.runs.length,
        bundles: plan.bundles.length,
        sessionDir: plan.sessionDir,
        artifactDir: plan.artifactDir,
        uploads: plan.uploads,
        holders: plan.holders.map((holder) => ({ jobId: holder.jobId, kind: holder.kind, how: holder.how })),
        known: plan.known
      } satisfies RetirePlan
    })

    return Service.of({ list, retirePlan })
  })
)

export * as SourceList from "./sources.ts"

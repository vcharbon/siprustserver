/**
 * The request builder: a selection the page states — the
 * filter bar's words, ticked cells, captures whole, the universe — previewed
 * and then frozen as one request PER LANE, through the same `freeze` the
 * command line uses (the request command's `create`), so the viewer names no call the
 * CLI would not. A preview is the resolution's own rule read before any row
 * exists; a create is `Requests.create`, one insert of N cells.
 *
 * A filter is the bar as it stood, and the bar stands on a lane: its words
 * are read on `selection.lane`'s queue — the page's — for every lane the
 * request targets, so the simulated lane's red calls freeze onto a hosted lane's request.
 * A filter naming no lane reads each target lane's own queue.
 * The prep is pinned once per call, the standing one, as a replay pins it.
 */
import * as Request from "@sip/corpus/requests/request"
import { pin } from "@sip/corpus/replay/plan"
import { createdBy } from "@sip/corpus/jobs/job-start"
import { Tracker } from "@sip/corpus"
import type { Selection as UniverseSelection } from "@sip/corpus/universe"
import { Calls, Cuts, Facets, Population, Requests, Runs } from "@sip/store"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { knobsOn } from "./lanes.ts"
import { pathsOf } from "./paths.ts"
import { SitePack } from "./site-pack.ts"

/** One cell as the page ticks it: by key, or by the case id the cut named it. */
export interface CellInput {
  readonly capture: string
  readonly callKey?: string | undefined
  readonly caseId?: string | undefined
}

/** A selection as the page states it: one form. */
export type Selection =
  | {
    readonly kind: "filter"
    /** The lane whose queue the words are read on: the bar's; absent, each target lane's own. */
    readonly lane?: string | undefined
    readonly scope?: string | undefined
    readonly q?: string | undefined
    readonly rule?: string | undefined
    readonly expected?: string | undefined
    readonly refusal?: string | undefined
    readonly outcome?: string | undefined
    readonly feature?: string | undefined
    readonly source?: string | undefined
    readonly "not-rule"?: ReadonlyArray<string> | undefined
    readonly "not-expected"?: ReadonlyArray<string> | undefined
    readonly "not-refusal"?: ReadonlyArray<string> | undefined
    readonly "not-outcome"?: ReadonlyArray<string> | undefined
    readonly "not-feature"?: ReadonlyArray<string> | undefined
    readonly "not-source"?: ReadonlyArray<string> | undefined
  }
  | { readonly kind: "captures"; readonly captures: ReadonlyArray<string> }
  | {
    readonly kind: "universe"
    readonly universe: "all" | "standing"
    readonly maxSpan?: number | undefined
    readonly greenOn?: string | undefined
    readonly sample?: number | undefined
    readonly limit?: number | undefined
  }
  | { readonly kind: "cells"; readonly cells: ReadonlyArray<CellInput> }

export interface PreviewRequest {
  readonly lanes: ReadonlyArray<string>
  readonly selection: Selection
  /** Captures the selection is widened to whole. */
  readonly wholeFirst?: ReadonlyArray<string> | undefined
}

/** The preview on one lane; the prep is the whole preview's. */
export interface LanePreview extends Omit<Request.Preview, "prepId"> {
  readonly lane: string
}

export interface Preview {
  readonly prepId: number
  readonly lanes: ReadonlyArray<LanePreview>
}

export interface CreateRequest extends PreviewRequest {
  readonly knobs?: Partial<Requests.Knobs> | undefined
}

export interface Created {
  readonly lane: string
  readonly requestId: number
  readonly cells: number
  readonly captures: ReadonlyArray<string>
}

export class Refused extends Schema.TaggedError<Refused>()("Site.RequestRefused", {
  detail: Schema.String
}) {}

export interface Interface {
  readonly preview: (request: PreviewRequest) => Effect.Effect<Preview, Refused>
  readonly create: (request: CreateRequest) => Effect.Effect<ReadonlyArray<Created>, Refused>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/RequestBuilder") {}

const blank = (value: string | undefined): string | undefined => (value === undefined || value === "" ? undefined : value)

const some = (values: ReadonlyArray<string> | undefined): ReadonlyArray<string> | undefined =>
  values === undefined || values.length === 0 ? undefined : values

/** The bar's words as the queue's own query, the scope and the outcome held to the words the queue knows. */
const filterQuery = (selection: Extract<Selection, { kind: "filter" }>): Facets.Query | Refused => {
  const scope = blank(selection.scope)
  if (scope !== undefined && !(Facets.SCOPES as ReadonlyArray<string>).includes(scope)) {
    return new Refused({ detail: `scope ${scope} is none of ${Facets.SCOPES.join(", ")}` })
  }
  const outcome = blank(selection.outcome)
  if (outcome !== undefined && !(Facets.OUTCOMES as ReadonlyArray<string>).includes(outcome)) {
    return new Refused({ detail: `outcome ${outcome} is none of ${Facets.OUTCOMES.join(", ")}` })
  }
  const exclude: Facets.Excludes = {
    ...(some(selection["not-rule"]) === undefined ? {} : { rule: selection["not-rule"] }),
    ...(some(selection["not-expected"]) === undefined ? {} : { expected: selection["not-expected"] }),
    ...(some(selection["not-refusal"]) === undefined ? {} : { refusal: selection["not-refusal"] }),
    ...(some(selection["not-outcome"]) === undefined
      ? {}
      : { outcome: selection["not-outcome"] as ReadonlyArray<Facets.Outcome> }),
    ...(some(selection["not-feature"]) === undefined ? {} : { feature: selection["not-feature"] }),
    ...(some(selection["not-source"]) === undefined ? {} : { source: selection["not-source"] })
  }
  return {
    scope: scope as Facets.Scope | undefined,
    capture: blank(selection.q),
    rule: blank(selection.rule),
    expected: blank(selection.expected),
    refusal: blank(selection.refusal),
    outcome: outcome as Facets.Outcome | undefined,
    feature: blank(selection.feature),
    source: blank(selection.source),
    ...(Object.keys(exclude).length === 0 ? {} : { exclude })
  }
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const laneCatalog = pack.lanes
    const cuts = yield* Cuts.Service
    const requests = yield* Requests.Service
    // What the scripts' freeze and preview read: handed to them as this layer holds it.
    const store = yield* Effect.context<
      Calls.Service | Cuts.Service | Population.Service | Runs.Service | Tracker.Service | Host
    >()

    /** The cells as keys: a case id is resolved off the standing cut of its capture at the prep. */
    const keyed = Effect.fn("RequestBuilder.keyed")(function*(cells: ReadonlyArray<CellInput>, prepId: number) {
      const byCase = cells.filter((cell) => blank(cell.callKey) === undefined)
      const cut = yield* cuts.cases(prepId, [...new Set(byCase.map((cell) => cell.capture))]).pipe(
        Effect.orDie
      )
      const out: Array<Requests.Cell> = []
      for (const cell of cells) {
        const key = blank(cell.callKey)
        if (key !== undefined) {
          out.push({ capture: cell.capture, callKey: key })
          continue
        }
        const caseId = blank(cell.caseId)
        if (caseId === undefined) {
          return new Refused({ detail: `a cell of ${cell.capture} names neither a call key nor a case id` })
        }
        const named = (cut.get(cell.capture) ?? []).find((one) => one.caseId === caseId)
        if (named === undefined) {
          return new Refused({ detail: `no cut as of prep ${prepId} holds ${caseId} of ${cell.capture}` })
        }
        out.push({ capture: cell.capture, callKey: named.callKey })
      }
      return out as ReadonlyArray<Requests.Cell>
    })

    /** The page's selection as the freeze takes it. */
    const inputOf = Effect.fn("RequestBuilder.inputOf")(function*(selection: Selection, prepId: number) {
      switch (selection.kind) {
        case "filter": {
          const query = filterQuery(selection)
          return query instanceof Refused
            ? query
            : { kind: "filter", query, lane: blank(selection.lane) } satisfies Request.SelectionInput
        }
        case "captures":
          return { kind: "captures", captures: selection.captures } satisfies Request.SelectionInput
        case "universe": {
          const universe: UniverseSelection = {
            universe: selection.universe,
            positional: [],
            onlyNew: false,
            maxSpanSeconds: selection.maxSpan,
            greenOnLane: blank(selection.greenOn),
            sampleSize: selection.sample,
            limit: selection.limit,
            cases: []
          }
          return { kind: "universe", selection: universe } satisfies Request.SelectionInput
        }
        case "cells": {
          const cells = yield* keyed(selection.cells, prepId)
          return cells instanceof Refused ? cells : { kind: "ticked", cells } satisfies Request.SelectionInput
        }
      }
    })

    /** The prep every capture is read as of: the standing one. */
    const pinned = Effect.gen(function*() {
      const prepId = yield* pin().pipe(Effect.provide(store), Effect.orDie)
      return prepId === null
        ? new Refused({ detail: `the cut store holds no cut — run \`${pack.triage.cut}\` first` })
        : prepId
    })

    /** The selection frozen on one lane and widened, or why not. */
    const frozenOn = Effect.fn("RequestBuilder.frozenOn")(function*(
      request: PreviewRequest,
      lane: string,
      prepId: number
    ) {
      const triage = config.triage
      if (triage === undefined) {
        return new Refused({ detail: `no triage block in ${config.configPath}: there is no store to build a request in` })
      }
      const input = yield* inputOf(request.selection, prepId)
      if (input instanceof Refused) return input
      const bare = yield* Request.freeze(pathsOf(pack, triage), lane, prepId, input).pipe(Effect.provide(store), Effect.orDie)
      const frozen = bare instanceof Request.Refused
        ? bare
        : yield* Request.widen(bare, prepId, request.wholeFirst ?? []).pipe(Effect.provide(store), Effect.orDie)
      return frozen instanceof Request.Refused ? new Refused({ detail: frozen.message }) : frozen
    })

    const lanesOf = (request: PreviewRequest): ReadonlyArray<string> | Refused => {
      const lanes = [...new Set(request.lanes.filter((lane) => lane !== ""))]
      return lanes.length === 0 ? new Refused({ detail: "a request names at least one lane" }) : lanes
    }

    const preview = Effect.fn("RequestBuilder.preview")(function*(request: PreviewRequest) {
      const lanes = lanesOf(request)
      if (lanes instanceof Refused) return yield* lanes
      const prepId = yield* pinned
      if (prepId instanceof Refused) return yield* prepId
      const previews: Array<LanePreview> = []
      for (const lane of lanes) {
        const frozen = yield* frozenOn(request, lane, prepId)
        if (frozen instanceof Refused) return yield* frozen
        const { prepId: _, ...shown } = yield* Request.preview(prepId, frozen).pipe(
          Effect.provide(store),
          Effect.orDie
        )
        previews.push({ ...shown, lane })
      }
      return { prepId, lanes: previews } satisfies Preview
    })

    const create = Effect.fn("RequestBuilder.create")(function*(request: CreateRequest) {
      const lanes = lanesOf(request)
      if (lanes instanceof Refused) return yield* lanes
      const prepId = yield* pinned
      if (prepId instanceof Refused) return yield* prepId
      const stated = request.knobs ?? {}
      // Every lane is frozen before any row is written: a create over three
      // lanes that refuses on the third leaves no half of itself behind.
      const frozen: Array<{ readonly lane: string; readonly frozen: Request.Frozen }> = []
      for (const lane of lanes) {
        const one = yield* frozenOn(request, lane, prepId)
        if (one instanceof Refused) return yield* one
        if (one.cells.length === 0 && one.selection.whole.length === 0) {
          return yield* new Refused({ detail: `the selection names no call on ${lane}` })
        }
        frozen.push({ lane, frozen: one })
      }
      const created: Array<Created> = []
      for (const { frozen: one, lane } of frozen) {
        const row = yield* requests.create({
          lane,
          selection: one.selection,
          knobs: knobsOn(laneCatalog, lane, stated),
          createdBy: yield* createdBy.pipe(Effect.provide(store)),
          cells: one.cells
        }).pipe(Effect.orDie)
        created.push({ lane, requestId: row.id, cells: row.cells, captures: row.captures })
      }
      return created as ReadonlyArray<Created>
    })

    return Service.of({ preview, create })
  })
)

export * as RequestBuilder from "./requests.ts"

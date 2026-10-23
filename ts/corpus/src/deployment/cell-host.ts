/**
 * `CellHost` — what a replay cell runs under in this deployment, and how a
 * landed cell is read back.
 *
 * The services: the interpreter adapter over the binary the replay names, the
 * deployment's lane presets and routing compiler, its classifier and its
 * reclassifier, with every bless the reclassifier makes filed to the sweep's
 * audit log, whose closing lines the replay prints once the sweep ends. How a
 * crate-test cell is invoked rides along, since the driver asks for it with
 * every cell.
 *
 * The readings: which of a cell's gating RFC charges its SUT answers for — the
 * charges it authored, not the ones it relayed from a peer — and the rule hits
 * of a cell that wrote no `rule-hits.json`.
 *
 * The default runs no cell, refusing by name when a sweep reaches for one,
 * and its readings state nothing.
 */
import type { CellHits, Confrontation } from "@sip/contracts"
import { LanePresets, RoutingCompiler, type TestInvocation } from "@sip/driver"
import { Classifier, Reclassifier } from "@sip/pipeline"
import { Bundle } from "@sip/store"
import { ReplayCli } from "@sip/toolchain"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import type * as Path from "effect/Path"
import * as Schema from "effect/Schema"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"

/** Every service a cell reaches for besides the platform. */
export type CellServices =
  | ReplayCli.Service
  | LanePresets.Service
  | RoutingCompiler.Service
  | Reclassifier.Service
  | Classifier.Service

/** The platform the cell services are built over. */
export type Platform = FileSystem.FileSystem | Path.Path | ChildProcessSpawner.ChildProcessSpawner

/** What one sweep's cells run under: the interpreter they exec, and the log every bless is filed to. */
export interface CellRequest {
  readonly interpreter: string
  readonly audit: string
}

/** The cell services could not be built; `detail` says why. */
export class CellHostUnready extends Schema.TaggedError<CellHostUnready>()("Corpus.CellHostUnready", {
  detail: Schema.String
}) {
  override get message(): string {
    return this.detail
  }
}

/**
 * The cell services, each refused with `detail`: what a deployment's
 * `services` falls back to where its own could not be built, so the sweep
 * fails naming why rather than with a cause no reader asked for.
 */
export const refusing = (detail: string): Layer.Layer<CellServices, CellHostUnready> => {
  const refused = Effect.fail(new CellHostUnready({ detail }))
  return Layer.mergeAll(
    Layer.effect(ReplayCli.Service, refused),
    Layer.effect(LanePresets.Service, refused),
    Layer.effect(RoutingCompiler.Service, refused),
    Layer.effect(Reclassifier.Service, refused),
    Layer.effect(Classifier.Service, refused)
  )
}

export interface Interface {
  /** How a crate-test cell is invoked. */
  readonly tests: Effect.Effect<TestInvocation, CellHostUnready>
  /** The services one sweep's cells run under. */
  readonly services: (request: CellRequest) => Layer.Layer<CellServices, CellHostUnready, Platform>
  /** The closing lines of the audit filed at `audit`; a log that does not read says so in a line. */
  readonly auditLines: (audit: string) => Effect.Effect<ReadonlyArray<string>, never, FileSystem.FileSystem>
  /** Which of a cell's gating RFC charges its SUT answers for. */
  readonly charges: Bundle.Charges
  /** The rule hits of a cell that wrote no `rule-hits.json`, off its confrontation records. */
  readonly hitsOf: (
    outcome: Reclassifier.RunOutcome,
    records: ReadonlyArray<Confrontation.ConfrontationRecord>
  ) => CellHits.Hits
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/CellHost") {}

const UNSTATED = "no cell host is composed — substitute CellHost.layerWith to say what a cell runs under"

/**
 * The default: no cell runs — its services and its crate invocation refuse
 * when a sweep reaches for them — and the readings state nothing: every
 * gating charge stands, a cell without its own counts has no rule hit, and no
 * audit was filed.
 */
export const layer = Layer.succeed(
  Service,
  Service.of({
    tests: Effect.fail(new CellHostUnready({ detail: UNSTATED })),
    services: () => refusing(UNSTATED),
    auditLines: () => Effect.succeed([]),
    charges: Bundle.everyCharge,
    hitsOf: () => []
  })
)

/** A deployment's own cell host. */
export const layerWith = (host: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(host))

/**
 * `Launcher` — how a job runs detached in this deployment: the command line of
 * the entry point each job kind runs, and the directory it runs in.
 *
 * A job's row records the command line it was launched with, so a restart
 * launches the same entry point: the deployment keeps each one at a stable
 * path.
 *
 * The default knows no entry point and refuses every launch by name.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"

/** The kinds of job a launch starts. */
export type JobKind = "replay" | "ingest" | "cut" | "retire" | "delete" | "campaign" | "inbox"

/** The deployment states no entry point for `kind`. */
export class LaunchUnstated extends Schema.TaggedError<LaunchUnstated>()("Corpus.LaunchUnstated", {
  kind: Schema.String
}) {
  override get message(): string {
    return `no entry point runs a ${this.kind} job — substitute Launcher.layerWith to say which`
  }
}

export interface Interface {
  /** The command line of `kind`'s entry point; the job's own arguments follow it. */
  readonly entry: (kind: JobKind) => Effect.Effect<ReadonlyArray<string>, LaunchUnstated>
  /** The directory a launched job runs in. */
  readonly cwd: string
  /** The command an operator runs to freeze a capture's earlier case ids, as a refusal names it. */
  readonly seedIds: string
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Launcher") {}

/** The default: no entry point. */
export const layer = Layer.sync(Service, () =>
  Service.of({
    entry: (kind) => Effect.fail(new LaunchUnstated({ kind })),
    cwd: process.cwd(),
    seedIds: "the deployment's case-id seed"
  }))

/** A deployment's own launcher. */
export const layerWith = (launcher: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(launcher))

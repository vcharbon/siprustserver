/**
 * `Launcher` — how a job runs detached in this deployment: the command line of
 * the entry point each job kind runs, the directory it runs in, and the shell
 * lines run before and after every job.
 *
 * A job's row records the command line it was launched with, so a restart
 * launches the same entry point: the deployment keeps each one at a stable
 * path. The hooks are read when a job asks for them, so an edit to where the
 * deployment states them is live for the next job.
 *
 * A hook never fails its job: the job is the work, the hook only the record
 * around it.
 *
 * The default knows no entry point and refuses every launch by name; it runs
 * no hook.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"

/** The kinds of job a launch starts. */
export type JobKind = "replay" | "ingest" | "cut" | "retire" | "delete"

/** One named shell line and the side of the job it runs on. */
export interface Hook {
  readonly name: string
  readonly when: "before" | "after"
  readonly command: string
}

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
  /** The directory the hooks run in. */
  readonly hooksCwd: string
  /** The hooks around every job, in the order they run. */
  readonly hooks: Effect.Effect<ReadonlyArray<Hook>>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Launcher") {}

/** The default: no entry point, no hook. */
export const layer = Layer.sync(Service, () =>
  Service.of({
    entry: (kind) => Effect.fail(new LaunchUnstated({ kind })),
    cwd: process.cwd(),
    hooksCwd: process.cwd(),
    hooks: Effect.succeed([])
  }))

/** A deployment's own launcher. */
export const layerWith = (launcher: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(launcher))

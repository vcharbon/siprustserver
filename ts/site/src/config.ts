/**
 * Viewer configuration: the only place a deployment's paths and commands
 * appear. The generic packages name no root and no runner; this file reads
 * both from a JSON file whose location comes from the environment, which is
 * the seam a deployment overloads, and the variables it reads beside it from
 * the site pack.
 */
import { Layout as WorkspaceLayout, PublicUrl } from "@sip/workspace"
import { Effect, Layer, Option, Result, Schema } from "effect"
import * as Config from "effect/Config"
import * as Context from "effect/Context"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { ViewerEnv } from "./env.ts"
import { SitePack } from "./site-pack.ts"

/** The variables the site's public URL, bind address and port are read from, and its port where none is stated. */
export const siteVariablesOf = (pack: Pick<SitePack.Pack, "lab">): PublicUrl.SiteVariables => ({
  ...pack.lab.workspace.variables,
  defaultPort: pack.lab.workspace.sitePort
})

const RootEntry = Schema.Struct({
  id: Schema.String,
  label: Schema.optional(Schema.String),
  /** Relative to the config file's own directory, or absolute. */
  path: Schema.String
})

const RunnerCommand = Schema.Struct({
  command: Schema.String,
  args: Schema.optional(Schema.Array(Schema.String)),
  cwd: Schema.optional(Schema.String)
})

/**
 * The triage deployment: where the verdict registry, the standing cut, the
 * per-capture corpus artifacts and the per-capture triage-session workdirs
 * live, plus the lane the queue reports on. The paths are paths because the
 * registry's ticket and evidence values are repo-relative, and only a
 * deployment knows where the repo is.
 *
 * The queue's enumeration is NOT here: it comes from the run database.
 */
const TriageEntry = Schema.Struct({
  /** The verdict registry (`registry.json`). */
  registry: Schema.String,
  /** Base the registry's repo-relative ticket and evidence paths resolve against. */
  repoRoot: Schema.String,
  /** Which lane's run status the queue reports; absent = the pack's default lane. */
  lane: Schema.optional(Schema.String),
  /** Postgres url for the run database; absent = `DATABASE_URL` (a workspace's `database.url`), else the package default. */
  database: Schema.optional(Schema.String),
  /** Directory holding one `<capture>/` of extraction artifacts per corpus capture. */
  corpus: Schema.String,
  /** Directory holding one `<capture-stem>/` triage-session workdir per triaged capture. */
  sessions: Schema.String,
  /**
   * The source command the capture-ingest endpoint runs, as
   * `<command> <args…> <file> | --set <file>… --cut`; it makes the ingest job
   * and returns. Absent = the pack's source command for the checkout at `repoRoot`.
   */
  ingest: Schema.optional(RunnerCommand),
  /** Where a dropped pcap is saved for the campaign to read; absent = `<corpus>/_uploads`. */
  uploads: Schema.optional(Schema.String)
})

/**
 * The command lines the app offers for copying, as this deployment spells
 * them: `triage <capture> [--case …] [--lane …]` hands a call to an agent,
 * `request` creates a replay request, `job` drives the job queue. A view
 * shows a command only where it is stated here.
 */
const CommandsEntry = Schema.Struct({
  triage: Schema.optional(Schema.String),
  request: Schema.optional(Schema.String),
  job: Schema.optional(Schema.String)
})

const File = Schema.Struct({
  caseRoots: Schema.Array(RootEntry),
  runRoots: Schema.optional(Schema.Array(RootEntry)),
  /** Where capture flows documents live: the source a captured case was cut from. */
  flowsRoots: Schema.optional(Schema.Array(RootEntry)),
  triage: Schema.optional(TriageEntry),
  port: Schema.optional(Schema.Number),
  /** Built SPA to serve when the viewer runs without Vite. */
  staticDir: Schema.optional(Schema.String),
  commands: Schema.optional(CommandsEntry)
})

export interface Runner extends Schema.Schema.Type<typeof RunnerCommand> {}

export interface Root {
  readonly id: string
  readonly label: string
  readonly path: string
}

export interface TriagePaths {
  readonly registry: string
  readonly repoRoot: string
  readonly lane: string
  readonly database: string | undefined
  readonly corpus: string
  readonly sessions: string
  /** The source command a drop runs: derived from `repoRoot` where unstated. */
  readonly ingest: Runner
  /** Where a dropped pcap lands before the campaign reads it. */
  readonly uploads: string
}

export interface Commands extends Schema.Schema.Type<typeof CommandsEntry> {}

export interface Values {
  readonly configPath: string
  readonly caseRoots: readonly Root[]
  readonly runRoots: readonly Root[]
  readonly flowsRoots: readonly Root[]
  readonly triage: TriagePaths | undefined
  readonly port: number
  /** The address the server binds: the pack's bind variable, else loopback (`@sip/workspace` `public-url.ts`). */
  readonly bind: string
  /** The origin every link the server prints or a report states is built under (`@sip/workspace` `public-url.ts`). */
  readonly publicUrl: string
  /** The base a campaign result's object links are built on (the pack's link base); undefined: the base each run stored. */
  readonly linkBase: string | undefined
  readonly staticDir: string | undefined
  /** The command lines the app offers; {} where the deployment states none. */
  readonly commands: Commands
  /** The workspace the pack's workspace variable names: its inbox and pcap root; absent outside one. */
  readonly workspace?: WorkspaceLayout.Base | undefined
}

export class Service extends Context.Service<Service, Values>()("@sip/site/Config") {}

export class LoadError extends Schema.TaggedError<LoadError>()("Site.ConfigLoadError", {
  path: Schema.String,
  detail: Schema.String
}) {
  override get message(): string {
    return `${this.path}: ${this.detail}`
  }
}

const decode = Schema.decodeUnknownEffect(File)

/**
 * Why a configuration `parsed` states a key the site no longer reads, or
 * `undefined`: `triage.traces` is gone, a cell's logs asked of an on-demand
 * collector from its call details instead.
 */
export const removedKeyOf = (parsed: unknown): string | undefined => {
  const triage = typeof parsed === "object" && parsed !== null ? (parsed as { readonly triage?: unknown }).triage : undefined
  const traces = typeof triage === "object" && triage !== null && "traces" in triage
  return traces
    ? "triage.traces: removed; a cell's logs are asked of an on-demand collector (lane-log) from its call details"
    : undefined
}

/** No root the environment states: what a config with no triage block reads. */
const NO_ROOTS: ViewerEnv.Roots = {
  sessions: undefined,
  corpus: undefined,
  port: undefined,
  bind: undefined,
  publicUrl: undefined,
  linkBase: undefined,
  database: undefined
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const pack = yield* SitePack.Service
    const variables = siteVariablesOf(pack)
    const configPath = yield* Config.string("SIPRIG_SITE_CONFIG").pipe(
      Config.withDefault("site.config.json")
    )
    const portOverride = yield* Config.option(Config.port(variables.port))

    const absolute = path.resolve(configPath)
    const base = path.dirname(absolute)
    const fail = (detail: string) => new LoadError({ path: absolute, detail })

    const text = yield* fs.readFileString(absolute).pipe(
      Effect.mapError((cause) => fail(String(cause)))
    )
    const parsed = yield* Effect.try({
      try: () => JSON.parse(text) as unknown,
      catch: (cause) => fail(`not JSON: ${String(cause)}`)
    })
    const removed = removedKeyOf(parsed)
    if (removed !== undefined) return yield* Effect.fail(fail(removed))
    const file = yield* decode(parsed).pipe(Effect.mapError((cause) => fail(String(cause))))

    const resolvePath = (value: string): string =>
      path.isAbsolute(value) ? value : path.resolve(base, value)
    const resolveRoot = (root: Schema.Schema.Type<typeof RootEntry>): Root => ({
      id: root.id,
      label: root.label ?? root.id,
      path: resolvePath(root.path)
    })

    const stated = file.triage
    const roots: ViewerEnv.Roots = stated === undefined
      ? NO_ROOTS
      : yield* ViewerEnv.roots(pack, resolvePath(stated.repoRoot)).pipe(
        Effect.mapError((refused) => fail(refused.reason))
      )

    /**
     * A root the environment states replaces a path the config states relative
     * to itself: relative is a default, which a task worktree resolves under a
     * tree nothing was ever written to. An absolute path is the deployment's
     * own and is kept.
     */
    const deployed = (stated: string, root: string | undefined): string =>
      root === undefined || path.isAbsolute(stated) ? resolvePath(stated) : root

    const triagePaths = (entry: Schema.Schema.Type<typeof TriageEntry>): TriagePaths => {
      const corpus = deployed(entry.corpus, roots.corpus)
      return {
        registry: resolvePath(entry.registry),
        repoRoot: resolvePath(entry.repoRoot),
        lane: entry.lane ?? pack.defaultLane,
        database: entry.database ?? roots.database,
        corpus,
        sessions: deployed(entry.sessions, roots.sessions),
        ingest: entry.ingest === undefined
          ? pack.ingestOf(resolvePath(entry.repoRoot))
          : {
            ...entry.ingest,
            cwd: entry.ingest.cwd === undefined ? base : resolvePath(entry.ingest.cwd)
          },
        uploads: entry.uploads === undefined
          ? path.join(corpus, "_uploads")
          : resolvePath(entry.uploads)
      }
    }

    // The provider's value is the caller's own; an env file's is read off the
    // record it was applied to, which the provider never sees again.
    const site = {
      [variables.bind]: Option.getOrUndefined(yield* Config.option(Config.string(variables.bind))) ?? roots.bind,
      [variables.publicUrl]: Option.getOrUndefined(yield* Config.option(Config.string(variables.publicUrl))) ??
        roots.publicUrl
    }
    const port = portOverride._tag === "Some"
      ? portOverride.value
      : roots.port === undefined
      ? (file.port ?? variables.defaultPort)
      : yield* Result.match(PublicUrl.portOf(variables, { [variables.port]: roots.port }), {
        onSuccess: Effect.succeed,
        onFailure: (reason) => Effect.fail(fail(reason))
      })
    const publicUrl = yield* PublicUrl.fromEnv(variables, site, port).pipe(Effect.mapError((e) => fail(e.reason)))
    const linkBase = yield* Result.match(
      pack.linkBase.of(
        Option.getOrUndefined(yield* Config.option(Config.string(pack.linkBase.variable))) ?? roots.linkBase
      ),
      { onSuccess: Effect.succeed, onFailure: (reason) => Effect.fail(fail(reason)) }
    )

    return Service.of({
      configPath: absolute,
      caseRoots: file.caseRoots.map(resolveRoot),
      runRoots: (file.runRoots ?? []).map(resolveRoot),
      flowsRoots: (file.flowsRoots ?? []).map(resolveRoot),
      triage: file.triage === undefined ? undefined : triagePaths(file.triage),
      port,
      bind: PublicUrl.bindOf(variables, site),
      publicUrl,
      linkBase,
      staticDir: file.staticDir === undefined ? undefined : resolvePath(file.staticDir),
      commands: file.commands ?? {},
      workspace: WorkspaceLayout.of(pack.lab.workspace, process.env)
    })
  })
)

export * as ViewerConfig from "./config.ts"

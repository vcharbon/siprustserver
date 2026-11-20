/**
 * The WORKSPACE: one directory holding every root and the database the
 * tooling uses, by fixed subdirectory names (the generic ones and the
 * tooling's own), configured by its one configuration file at its root. The
 * workspace variable names it; unset, nothing here applies and every root
 * keeps its checkout default.
 *
 * A workspace states its roots as environment defaults ({@link defaultsOf}):
 * an exported variable still wins, and the checkout's env files lose to the
 * workspace (`workspace-env.ts`).
 */
import * as path from "node:path"
import type * as ConfigFile from "./config-file.js"
import { DATABASE_URL, type WorkspacePack } from "./workspace-pack.js"

/** The generic fixed subdirectories of a workspace, by role. */
export const DIRECTORIES = {
  artifacts: "artifacts",
  runs: "runs",
  census: "census",
  pcaps: "pcaps",
  inbox: "inbox",
  seeds: ".seeds",
  campaigns: "campaigns",
  backups: "backups",
  logs: "logs",
  locks: "locks",
  targets: "targets",
  lanes: "lanes",
  pgdata: "pgdata",
  state: "state"
} as const

/** The places every workspace holds, absolute. */
export interface Base {
  readonly root: string
  readonly config: string
  /** The artifact store. */
  readonly artifacts: string
  /** The session tree. */
  readonly runs: string
  /** The RFC-violation census runs. */
  readonly census: string
  /** The one pcap root. */
  readonly pcaps: string
  /** Where a pcap is dropped before it is anonymised into {@link pcaps}. */
  readonly inbox: string
  /** The anonymisation seeds, one per pcap. */
  readonly seeds: string
  /** The user's campaign files. */
  readonly campaigns: string
  /** The store's backups, one dump per migration that needed one. */
  readonly backups: string
  readonly logs: string
  /** The lock root. */
  readonly locks: string
  /** The SIP and backend targets a lane names. */
  readonly targets: string
  readonly lanes: string
  /** The workspace's own postgres cluster. */
  readonly pgdata: string
  /** What the services a run stands keep while they run. */
  readonly state: string
}

/** Every place a workspace of a tooling adding the directories `D` holds, absolute. */
export type Layout<D extends string = never> = Base & { readonly [K in D]: string }

/** The store's database where `database.url` states none: the workspace's own postgres over TCP. */
export const databaseUrlOf = (pack: Pick<WorkspacePack, "database" | "databasePort">): string =>
  `postgresql://127.0.0.1:${pack.databasePort}/${pack.database}`

/** The places of the workspace at `root`, resolved against `cwd` where relative. */
export const layoutOf = <D extends string>(pack: WorkspacePack<D>, root: string, cwd: string = process.cwd()): Layout<D> => {
  const at = path.resolve(cwd, root)
  const roles = Object.entries({ ...DIRECTORIES, ...pack.directories }).map(([role, name]) => [role, path.join(at, name)])
  return { root: at, config: path.join(at, pack.configFile), ...Object.fromEntries(roles) } as Layout<D>
}

/** The workspace `env` names, or `undefined` where it names none. */
export const of = <D extends string>(pack: WorkspacePack<D>, env: NodeJS.ProcessEnv = process.env): Layout<D> | undefined => {
  const named = env[pack.variables.workspace]?.trim()
  return named === undefined || named === "" ? undefined : layoutOf(pack, named)
}

/**
 * The environment a workspace states, in the variables every tool already
 * reads: its fixed roots, no extra pcap root (so `pcaps/` is the only one an
 * env file can reach), then what its configuration file sets (a relative
 * path there taken against the workspace root), the store's database
 * defaulting to the workspace's own postgres.
 */
export const defaultsOf = (pack: WorkspacePack, layout: Base, config: ConfigFile.Base): Readonly<Record<string, string>> => {
  const vars = pack.variables
  const stated: Record<string, string> = {
    [vars.artifacts]: layout.artifacts,
    [vars.sessions]: layout.runs,
    [vars.census]: layout.census,
    [vars.pcaps]: layout.pcaps,
    [vars.pcapRoots]: "",
    [vars.locks]: layout.locks,
    [DATABASE_URL]: config.database?.url ?? databaseUrlOf(pack)
  }
  if (config.site?.port !== undefined) stated[vars.port] = String(config.site.port)
  if (config.site?.bind !== undefined) stated[vars.bind] = config.site.bind
  if (config.site?.public_url !== undefined) stated[vars.publicUrl] = config.site.public_url
  if (config.tools?.bin_dir !== undefined) stated[vars.binDir] = path.resolve(layout.root, config.tools.bin_dir)
  return stated
}

/** Every function above bound to `pack`, its arguments otherwise the same. */
export const make = <D extends string>(pack: WorkspacePack<D>) => ({
  layoutOf: (root: string, cwd?: string) => layoutOf(pack, root, cwd),
  of: (env?: NodeJS.ProcessEnv) => of(pack, env),
  defaultsOf: (layout: Base, config: ConfigFile.Base) => defaultsOf(pack, layout, config)
})

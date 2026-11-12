/**
 * The host facts a campaign needs and an operator's shell may not carry: the
 * env files kept beside the checkout, among them the systemd user session the
 * pipeline's long children run in where one answers (`user-session.ts`).
 *
 * A file is a DEFAULT, never an override: a key already exported is left
 * exactly as the caller set it, and only the keys a tooling states are applied
 * at all ({@link keysOf}), so a file cannot smuggle an environment past what
 * the tooling documents. `.env.local` first, then `.env.<hostname>`, and the
 * first file to state a key wins — the local file is the one an operator edits.
 *
 * Nothing here is host-named: a hostname only ever appears as the name of a
 * file a box happens to hold. A path a file states is taken relative to the
 * file's own directory — the checkout — so `../<sibling>` names the sibling
 * on every box the checkout lands on; an absolute path is kept as is.
 *
 * A workspace is applied before the files and so wins over them; it is named
 * by the caller's environment only.
 */
import type { HostEnvFacts } from "@sip/cli"
import * as HostEnv from "@sip/cli/host-env"
import * as os from "node:os"
import * as path from "node:path"
import type { LabPack } from "./lab-pack.js"

/** The keys every tooling's env files may set, beside its own: the store and the user session. */
export const BASE_KEYS: Readonly<Record<string, string>> = {
  DATABASE_URL: "the result store",
  STORE_TEST_URL: "the postgres server the DB-bound test suites build their databases on",
  XDG_RUNTIME_DIR: "the systemd user session long children run in",
  DBUS_SESSION_BUS_ADDRESS: "the same session's bus",
  NO_PROXY: "hosts the box must reach directly"
}

/** What a tooling adds to the generic keys: its own, and those of them that name a place on disk. */
export interface OwnKeys {
  readonly keys: Readonly<Record<string, string>>
  readonly pathKeys: ReadonlyArray<string>
}

/**
 * The keys an env file may set under `pack`, and what each one is for: the
 * environment's name, {@link BASE_KEYS}, the workspace's root and site
 * variables, then the tooling's own. A key outside them is ignored with a line
 * saying so, because a typo in an env file is otherwise silence.
 */
export const keysOf = (pack: Pick<LabPack, "workspace" | "variables">, own: OwnKeys): Readonly<Record<string, string>> => {
  const v = pack.workspace.variables
  return {
    [pack.variables.environment]: "what this checkout is: `integration`, or the name of a task worktree",
    ...BASE_KEYS,
    [v.artifacts]: "the artifact store root",
    [v.sessions]: "the session tree root",
    [v.census]: "the RFC-violation census runs the cut reads",
    [v.pcaps]: "the default --pcaps source",
    [v.pcapRoots]: "extra roots a source may be added from, `:`-separated",
    [v.binDir]: "the prebuilt binary directory; set, nothing is built",
    [v.locks]: "the box-wide lock root",
    [v.publicUrl]: "the site's public URL every printed link is built under",
    [v.bind]: "the address the site binds",
    [v.port]: "the site's port",
    ...own.keys
  }
}

/** The keys whose value is a place on disk, resolved against the file that states them. */
export const pathKeysOf = (pack: Pick<LabPack, "workspace">, own: OwnKeys): ReadonlySet<string> => {
  const v = pack.workspace.variables
  return new Set([v.artifacts, v.sessions, v.census, v.pcaps, v.binDir, v.locks, ...own.pathKeys])
}

/** The files a box may keep beside the checkout `repo`, in the order they are read. */
export const files = (repo: string): ReadonlyArray<string> => [
  path.join(repo, ".env.local"),
  path.join(repo, `.env.${os.hostname().toLowerCase()}`)
]

/**
 * The host env a checkout keeps, as `@sip/cli` applies it: `before` first
 * (the workspace applied to the environment), then the files.
 */
export const hostEnv = (
  pack: Pick<LabPack, "workspace" | "variables">,
  own: OwnKeys,
  repo: string,
  before: HostEnvFacts["before"]
): HostEnvFacts => ({
  files: files(repo),
  keys: keysOf(pack, own),
  pathKeys: pathKeysOf(pack, own),
  before
})

/** The environment name the integration checkout carries; the campaign runs nowhere else, its dry run apart. */
export const INTEGRATION = "integration"

/**
 * Why this checkout may not drive `what` (the campaign, the replay bench), or
 * `null`. A task worktree is for the default test lane; the corpus, the lanes'
 * fixed ports and the result store are the integration checkout's, so a
 * worktree that names itself anything but `integration` (the pack's
 * `environment` variable) is refused by name. A checkout that states no name
 * is the integration checkout of a box that predates the key.
 */
export const environmentRefusal = (
  pack: Pick<LabPack, "variables">,
  env: NodeJS.ProcessEnv = process.env,
  what = "the campaign"
): string | null => {
  const name = env[pack.variables.environment]
  if (name === undefined || name === "" || name === INTEGRATION) return null
  return `this is environment '${name}', not the integration checkout: ${what} runs from the integration checkout only`
}

/** The env files over `pack`, the tooling's own keys added and `before` applied first. */
export const make = (pack: Pick<LabPack, "workspace" | "variables">, own: OwnKeys, before: HostEnvFacts["before"]) => {
  const hostEnvOf = (repo: string) => hostEnv(pack, own, repo, before)
  return {
    KEYS: keysOf(pack, own),
    PATH_KEYS: pathKeysOf(pack, own),
    hostEnv: hostEnvOf,
    apply: (repo: string, env: NodeJS.ProcessEnv = process.env) => HostEnv.apply(hostEnvOf(repo), env),
    environmentRefusal: (env: NodeJS.ProcessEnv = process.env, what = "the campaign") =>
      environmentRefusal(pack, env, what)
  }
}

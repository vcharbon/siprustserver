/**
 * The roots this box states, read off the workspace and the checkout's env
 * files the way every entry point of the pack reads them (the pack's
 * `applyEnv`: defaults, never overrides). Applying them to the process is half
 * the point: the census root and the store url the server itself never
 * resolves are what the jobs it spawns inherit, so a task worktree's site and
 * its jobs read one box.
 */
import * as Effect from "effect/Effect"
import type { SitePack } from "./site-pack.ts"

/**
 * The roots a config's relative path defers to, the site's port, bind address
 * and public URL, the base of the campaign results' object links, and the
 * store's database; an unstated one leaves its default alone.
 */
export interface Roots {
  readonly sessions: string | undefined
  readonly corpus: string | undefined
  readonly port: string | undefined
  readonly bind: string | undefined
  readonly publicUrl: string | undefined
  readonly linkBase: string | undefined
  readonly database: string | undefined
}

/** The variable the store's database url is read from. */
export const DATABASE_URL = "DATABASE_URL"

/**
 * Applies the pack's env files under `repo` to `env` (the workspace its
 * workspace variable names first), as every entry point of the pack applies
 * them, and reads the roots off it by the pack's variables: the jobs a site
 * spawns inherit the roots its box states, never the ones a worktree holds.
 */
export const roots = (pack: Pick<SitePack.Pack, "applyEnv" | "lab" | "linkBase">, repo: string, env: NodeJS.ProcessEnv = process.env) =>
  Effect.map(pack.applyEnv(repo, env), (): Roots => {
    const variables = pack.lab.workspace.variables
    return {
      sessions: env[variables.sessions],
      corpus: env[variables.artifacts],
      port: env[variables.port],
      bind: env[variables.bind],
      publicUrl: env[variables.publicUrl],
      linkBase: env[pack.linkBase.variable],
      database: env[DATABASE_URL]
    }
  })

export * as ViewerEnv from "./env.ts"

/**
 * Where a deployment keeps its triage things: the roots' defaults its pack
 * states and the host variables that override them, and the session tree's
 * `_rules` and `_falsify`. The naming conventions every phase shares (a
 * capture is a pcap basename, its session workdir that basename with the
 * suffix off, the cut logs under `_preps`) are `@sip/corpus` `paths.ts`.
 *
 * The artifact store (`corpus`), the session tree and the census are box-wide:
 * the first two overridable per invocation and per host, the census per host.
 */
import type * as Corpus from "@sip/corpus/paths"
import * as path from "node:path"
import type { LabPack } from "./lab-pack.js"

/** The roots. */
export type Paths = Corpus.Paths

/** The roots an invocation states; an absent one is the default. */
export type Overrides = { readonly [K in keyof Paths]?: string | undefined }

/** Where a whole-roster pass leaves its report for the viewer to read. */
export const RULES = "_rules"

/** The rules directory of a session tree; the viewer reads what a pass left here. */
export const rulesDir = (sessions: string): string => path.join(sessions, RULES)

/**
 * Where a falsifier pass keeps its scratch: on the session tree's disk, never
 * the temp dir, and never inside the artifact store, whose census walk would
 * read the enriched flows documents the pass writes.
 */
export const FALSIFY = "_falsify"

/** The parent of every falsifier pass's own scratch directory. */
export const falsifyDir = (sessions: string): string => path.join(sessions, FALSIFY)

/** The roots where neither the invocation nor the host states one: the pack's, the checkout its own. */
export const defaultsOf = (pack: Pick<LabPack, "roots" | "checkout">): Paths => ({
  corpus: pack.roots.corpus,
  sessions: pack.roots.sessions,
  census: pack.roots.census,
  registry: pack.roots.registry,
  repoRoot: pack.checkout
})

/**
 * The roots as this invocation has them: what it stated, else what this host's
 * environment states (the workspace's `artifacts`, `sessions` and `census`
 * variables), else the pack's default. The environment's value is, first to
 * last, the caller's exported variable, the workspace's fixed subdirectory,
 * then the checkout's env files (`env-file.ts`). Read at CALL time, so a
 * command that applies its env files on the way in is answered with them.
 */
export const resolve = (
  pack: Pick<LabPack, "roots" | "checkout" | "workspace">,
  overrides: Overrides,
  env: NodeJS.ProcessEnv = process.env
): Paths => {
  const defaults = defaultsOf(pack)
  const variables = pack.workspace.variables
  return {
    corpus: path.resolve(overrides.corpus ?? env[variables.artifacts] ?? defaults.corpus),
    sessions: path.resolve(overrides.sessions ?? env[variables.sessions] ?? defaults.sessions),
    census: path.resolve(overrides.census ?? env[variables.census] ?? defaults.census),
    registry: path.resolve(overrides.registry ?? defaults.registry),
    repoRoot: path.resolve(overrides.repoRoot ?? defaults.repoRoot)
  }
}

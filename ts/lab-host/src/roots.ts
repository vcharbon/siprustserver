/**
 * A deployment's ROOTS (`@sip/corpus` `Roots`): the pack's roots
 * (`paths.ts`) unless the host states its own (the workspace's `artifacts`,
 * `sessions` and `census` variables); the pcap roots a source may be added
 * from — the `pcaps` variable's, the checkout's siblings and the artifact
 * store's uploads outside a workspace, and every extra root the `pcapRoots`
 * variable names (`:`-separated, relative to the checkout); the box-wide lock
 * root, the `locks` variable's where it is set, else beside the box's other
 * runtime state; the pack's bench. Each is read off the environment when a
 * command asks for it.
 */
import { Roots } from "@sip/corpus"
import { Layout } from "@sip/workspace"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as path from "node:path"
import type { LabPack } from "./lab-pack.js"
import { resolve } from "./paths.js"

/**
 * The box-wide locks: outside every checkout, beside the box's other runtime
 * state, at the pack's `locksUnder` in `$XDG_RUNTIME_DIR`, `/run/user/<uid>`
 * where the session states no runtime dir. Never the temp dir, which differs
 * between processes of one user.
 */
export const locksRoot = (pack: Pick<LabPack, "workspace" | "locksUnder">, env: NodeJS.ProcessEnv): string =>
  env[pack.workspace.variables.locks] ??
    path.join(env["XDG_RUNTIME_DIR"] ?? `/run/user/${process.getuid!()}`, pack.locksUnder)

/**
 * The roots a source pcap may lie under besides the store's uploads, absolute,
 * as `env` names them; in a workspace the checkout's siblings are none of them.
 */
export const pcapRoots = (pack: Pick<LabPack, "workspace" | "checkout">, env: NodeJS.ProcessEnv): ReadonlyArray<string> => {
  const variables = pack.workspace.variables
  const named: Array<string> = Layout.of(pack.workspace, env) === undefined ? [path.dirname(pack.checkout)] : []
  const pcaps = env[variables.pcaps]
  if (pcaps !== undefined && pcaps !== "") named.unshift(path.resolve(pack.checkout, pcaps))
  for (const extra of (env[variables.pcapRoots] ?? "").split(":")) {
    if (extra !== "") named.push(path.resolve(pack.checkout, extra))
  }
  return named
}

/**
 * Whether the artifact store's uploads are a source root: outside a
 * workspace; in one, a capture enters only through its inbox into `pcaps/`.
 */
export const takesUploads = (pack: Pick<LabPack, "workspace">, env: NodeJS.ProcessEnv): boolean =>
  Layout.of(pack.workspace, env) === undefined

/** The roots over `pack`, each read off this process's environment when asked. */
export const layer = (pack: LabPack) =>
  Layer.succeed(
    Roots.Service,
    Roots.Service.of({
      paths: (overrides) => Effect.sync(() => resolve(pack, overrides)),
      pcapRoots: Effect.sync(() => pcapRoots(pack, process.env)),
      uploadsRoot: Effect.sync(() => takesUploads(pack, process.env)),
      locks: Effect.sync(() => locksRoot(pack, process.env)),
      bench: Effect.succeed(pack.roots.bench)
    })
  )

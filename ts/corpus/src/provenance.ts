/**
 * Which code produced a cut or a run here: every repository the deployment's
 * `Identity` names, read as `@sip/toolchain` `Git` reads a checkout — its
 * commit, whether its tree was dirty, and its fingerprint.
 *
 * Read once per process: a sweep's thousands of runs share one tree, and the
 * code a process runs is the code it started with.
 */
import type { Model } from "@sip/store"
import { Git } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { Identity } from "./deployment/index.js"

let held: Model.Provenance | undefined

/** The code of every repository the deployment names, in the order it names them. */
export const current: Effect.Effect<Model.Provenance, never, Identity.Service | ChildProcessSpawner.ChildProcessSpawner> =
  Effect.suspend(() => {
    if (held !== undefined) return Effect.succeed(held)
    return Effect.gen(function*() {
      const identity = yield* Identity.Service
      const code = yield* Effect.forEach(identity.repositories, (one) => Git.componentOf(one.name, one.dir))
      held = code
      return code
    })
  })

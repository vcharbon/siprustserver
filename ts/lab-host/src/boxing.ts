/**
 * A deployment's BOXING (`@sip/corpus` `Boxing`): a `systemd-run --user
 * --scope` with `MemoryMax`, no swap and a `CPUQuota` where a user session
 * answers, plain exec where none does (`user-session.ts`); a cell's
 * interpreter runs through its pack's wrapper, its scope sized by the
 * wrapper's variable, which is stated only where a session answers.
 */
import { Boxing } from "@sip/corpus"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { boxVariables } from "./interpreter-env.js"
import type { LabPack } from "./lab-pack.js"
import * as UserSession from "./user-session.js"

/** `request`'s command line, inside a capped user scope where `session` holds. */
export const box = (request: Boxing.BoxRequest, session: boolean): Boxing.CommandLine =>
  UserSession.scoped(
    [`MemoryMax=${request.memoryMax}`, "MemorySwapMax=0", `CPUQuota=${request.cpus * 100}%`],
    request.command,
    request.args,
    session
  )

/** The boxing of a host where a user session answers (`session`) or does not. */
export const layerFor = (pack: Pick<LabPack, "interpreter">, session: boolean) =>
  Boxing.layerWith({
    box: (request) => box(request, session),
    refusal: Effect.succeed(null),
    interpreter: Effect.succeed(pack.interpreter.wrapper),
    interpreterVariables: (memoryMax) => boxVariables(pack, memoryMax, session)
  })

/** The boxing of this host, its session asked when the layer is built. */
export const layer = (pack: Pick<LabPack, "interpreter" | "variables">) =>
  Layer.unwrap(Effect.map(UserSession.answers(pack.variables.userSession), (session) => layerFor(pack, session)))

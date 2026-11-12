/**
 * What a deployment's interpreter child of a cell runs under, beyond this
 * process's own environment: the memory box around the Rust process (its
 * pack's wrapper, sized by the wrapper's variable where a user session
 * answers, `user-session.ts`) and the ledger every case in flight has a row
 * in — what `@sip/corpus` reads off `Boxing.interpreterVariables` and
 * `CaseLayout.interpreterVariable` for a replay, stated here for the passes
 * that drive cells themselves.
 */
import { ReplayCli } from "@sip/toolchain"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import type * as Layer from "effect/Layer"
import type { LabPack } from "./lab-pack.js"
import * as UserSession from "./user-session.js"

export interface InterpreterEnv {
  /** The ceiling the box around the Rust process is capped at. */
  readonly memoryMax: string
  /** The one directory holding a row for every number the cells dial. */
  readonly ledgerDir?: string
}

/**
 * The box's own variable: what the wrapper sizes its scope by, stated where a
 * user session answers (`session`); none where it does not, and the wrapper
 * then execs the interpreter plainly.
 */
export const boxVariables = (
  pack: Pick<LabPack, "interpreter">,
  memoryMax: string,
  session: boolean
): Readonly<Record<string, string>> => session ? { [pack.interpreter.memoryVariable]: memoryMax } : {}

/** The record as the interpreter's own variables name it, the box stated where `session` holds. */
export const envBlock = (
  pack: Pick<LabPack, "interpreter">,
  env: InterpreterEnv,
  session: boolean
): Readonly<Record<string, string>> => ({
  ...boxVariables(pack, env.memoryMax, session),
  ...(env.ledgerDir === undefined ? {} : { [pack.interpreter.ledgerVariable]: env.ledgerDir })
})

/** `self`, with every interpreter it spawns given `env`, the box stated where this process's session answers. */
export const withInterpreterEnv = (pack: Pick<LabPack, "interpreter" | "variables">, env: InterpreterEnv) =>
<A, E, R>(self: Effect.Effect<A, E, R>): Effect.Effect<A, E, R> =>
  Effect.flatMap(
    UserSession.answers(pack.variables.userSession),
    (session) => Effect.provideService(self, ReplayCli.Environment, envBlock(pack, env, session))
  )

/** The interpreter the cells exec: `REPLAY_BIN` named to the pack's wrapper ahead of anything the environment says. */
export const boxedInterpreter = (pack: Pick<LabPack, "interpreter">): Layer.Layer<never> =>
  ConfigProvider.layerAdd(
    ConfigProvider.fromUnknown({ REPLAY_BIN: pack.interpreter.wrapper }),
    { asPrimary: true }
  )

/** The interpreter environment over `pack`'s facts. */
export const make = (pack: Pick<LabPack, "interpreter" | "variables">) => ({
  boxVariables: (memoryMax: string, session: boolean) => boxVariables(pack, memoryMax, session),
  envBlock: (env: InterpreterEnv, session: boolean) => envBlock(pack, env, session),
  withInterpreterEnv: (env: InterpreterEnv) => withInterpreterEnv(pack, env),
  boxedInterpreter: boxedInterpreter(pack)
})

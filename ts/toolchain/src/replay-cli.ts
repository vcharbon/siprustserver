/**
 * `replay` — one run-spec in, one run bundle out, the exit code the structural
 * verdict.
 *
 * The exit vocabulary IS the contract, so this adapter maps it and stops there:
 * 0 passed, 1 the run failed (verdict or post-run cleanup), 2 refused (lint /
 * compile / spec), 3 environment, 4 the run body panicked. The bundle is on
 * disk either way — even a panicked run leaves the ladder under a `RunUnwound`
 * verdict — so the caller reads it with `@sip/contracts`, and nothing here
 * tries to summarise what it will find.
 *
 * The binary is built by the DEPLOYMENT, not by this repository, so its path
 * comes from `REPLAY_BIN` with no default: guessing a path outside this
 * checkout would mean running whatever happened to be there.
 */
import type * as Config from "effect/Config"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { REPLAY } from "./binaries.js"
import { type CliFailed, type CliUnavailable, runner } from "./spawn.js"

/** What one `replay` invocation decided, in the binary's own vocabulary. */
export type RunOutcome =
  /** Exit 0: green verdict AND clean post-run state. */
  | { readonly _tag: "passed" }
  /** Exit 1: the run produced a verdict, and it is not a passing one. */
  | { readonly _tag: "failed" }
  /** Exit 2: the document or the spec refuses this run. */
  | { readonly _tag: "refused" }
  /** Exit 3: the host failed the run — an unreadable file, a bind, a write. */
  | { readonly _tag: "environment" }
  /** Exit 4: the run body panicked; the armed bundle writer still committed. */
  | { readonly _tag: "panicked" }
  /** An exit code the vocabulary does not name. Never seen from a healthy binary. */
  | { readonly _tag: "unknown"; readonly exitCode: number }

const OUTCOMES: ReadonlyArray<RunOutcome["_tag"]> = ["passed", "failed", "refused", "environment", "panicked"]

/** The outcome an exit code names. */
export const outcomeOf = (exitCode: number): RunOutcome => {
  const tag = OUTCOMES[exitCode]
  return tag === undefined ? { _tag: "unknown", exitCode } : ({ _tag: tag } as RunOutcome)
}

/** Whether the run reached a bundle a reader can judge — every outcome but a host failure. */
export const leftABundle = (outcome: RunOutcome): boolean =>
  outcome._tag === "passed" || outcome._tag === "failed" || outcome._tag === "panicked"

/** One finished `replay` invocation. */
export interface RunReport {
  readonly outcome: RunOutcome
  readonly exitCode: number
  /** The binary's own one-line summary, where it wrote one. */
  readonly stdout: string
  /** Why it refused, or what unwound. Empty on a clean pass. */
  readonly stderr: string
}

export interface Interface {
  /** `replay <run-spec.json>` — run one case on one lane and report the exit. */
  readonly run: (runSpecPath: string) => Effect.Effect<RunReport, CliUnavailable>
  /** `replay schema` — the run-spec JSON Schema a driver emits against. */
  readonly schema: () => Effect.Effect<string, CliUnavailable | CliFailed>
}

export class Service extends Context.Service<Service, Interface>()("@sip/toolchain/ReplayCli") {}

/**
 * The variables the interpreter process is given over this process's own
 * environment. Empty unless a driver provides one around the cells whose
 * interpreter needs facts it cannot take off the command line; nothing in
 * this process reads it back.
 */
export const Environment: Context.Reference<Readonly<Record<string, string>>> = Context.Reference(
  "@sip/toolchain/ReplayCli/Environment",
  { defaultValue: () => ({}) }
)

export const layer = Layer.effect(
  Service,
  Effect.gen(function* () {
    const cli = yield* runner
    const bin = yield* REPLAY

    const runSpec = Effect.fn("ReplayCli.run")(function* (runSpecPath: string) {
      const env = yield* Environment
      const output = yield* cli.all(bin, [runSpecPath], { env: { ...env } })
      return {
        outcome: outcomeOf(output.exitCode),
        exitCode: output.exitCode,
        stdout: output.stdout,
        stderr: output.stderr
      } satisfies RunReport
    })

    const schema = Effect.fn("ReplayCli.schema")(function* () {
      return yield* cli.ok(bin, ["schema"])
    })

    return Service.of({ run: runSpec, schema })
  })
)

/** The binary this service will run. Configured, never guessed. */
export const binary: Config.Config<string> = REPLAY

export * as ReplayCli from "./replay-cli.js"

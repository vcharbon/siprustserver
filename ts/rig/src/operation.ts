/**
 * The operation manifest, `operations/<name>/operation.toml` (`operation.v1`,
 * ADR-0060 appendix §7): what changes the lab and restores it, with the
 * inject, heal and probe rules of ADR-0049 §2.
 */
import * as Schema from "effect/Schema"
import { Name, Params, parseWith, RelativePath, Seconds } from "./manifest-file.js"

/** The schema version an operation manifest states. */
export const SCHEMA = "operation.v1"

/** The manifest's file name in its directory. */
export const FILE = "operation.toml"

const Timeouts = Schema.Struct({
  inject_s: Schema.optionalKey(Seconds),
  heal_s: Schema.optionalKey(Seconds),
  probe_s: Schema.optionalKey(Seconds)
})

const Run = Schema.Struct({
  inject: RelativePath,
  /** Always runs, and is idempotent. */
  heal: RelativePath,
  /** Proves recovery after `heal`: exit 0 recovered, 75 not yet, anything else failed. */
  probe: Schema.optionalKey(RelativePath)
})

export const Operation = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  /** Its directory's name. */
  name: Name,
  /** The lane parts it needs: a lane lacking one cannot run a case asking for it. */
  needs: Schema.optionalKey(Schema.Array(Name)),
  /** Box-wide locks held from `inject` to `heal`. */
  locks: Schema.optionalKey(Schema.Array(Name)),
  params: Schema.optionalKey(Params),
  timeouts: Schema.optionalKey(Timeouts),
  run: Run
})
export interface Operation extends Schema.Schema.Type<typeof Operation> {}

/** The executables an operation names, by the step that runs each. */
export const executablesOf = (operation: Operation): ReadonlyArray<readonly [string, string]> =>
  (["inject", "heal", "probe"] as const).flatMap((step) => {
    const file = operation.run[step]
    return file === undefined ? [] : [[step, file] as const]
  })

/** The operation manifest `file` holds as `text`, or why it is none. */
export const parse = parseWith(Operation, () => undefined)

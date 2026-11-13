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
  /** The bound of the whole probe: a `probe` answering "not yet" is called again within it. */
  probe_s: Schema.optionalKey(Seconds),
  /** The pause between two calls of a `probe` that answered "not yet". */
  probe_interval_s: Schema.optionalKey(Seconds)
})

/** Each timeout of an operation, in seconds. */
export interface Bounds {
  readonly inject_s: number
  readonly heal_s: number
  readonly probe_s: number
  readonly probe_interval_s: number
}

/** The timeouts an operation takes where its manifest states none. */
export const DEFAULT_BOUNDS: Bounds = { inject_s: 60, heal_s: 120, probe_s: 60, probe_interval_s: 1 }

/** The exit status of a `probe` that answers "not yet" (`EX_TEMPFAIL`). */
export const NOT_YET = 75

const Run = Schema.Struct({
  inject: RelativePath,
  /** Always runs, and is idempotent. */
  heal: RelativePath,
  /** Proves recovery after `heal`: exit 0 recovered, 75 not yet, anything else failed. */
  probe: Schema.optionalKey(RelativePath)
})

/** What an inject may stop that the lane's orchestrator stands again: the lane's managed system under test. */
export const STOPS = ["system"] as const

/**
 * The file under `SIPRIG_OUT` an inject of an operation that `stops` the
 * system names the process it stopped in (its pid, one line). Its death alone
 * is the step's doing; any other death of the system stays the batch's.
 */
export const STOPPED_FILE = "stopped"

export const Operation = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  /** Its directory's name. */
  name: Name,
  /** The lane parts it needs: a lane lacking one cannot run a case asking for it. */
  needs: Schema.optionalKey(Schema.Array(Name)),
  /** What its inject stops, its orchestrator standing it again ({@link STOPPED_FILE}). */
  stops: Schema.optionalKey(Schema.Literals(STOPS)),
  /** Box-wide locks held from `inject` to `heal`. */
  locks: Schema.optionalKey(Schema.Array(Name)),
  params: Schema.optionalKey(Params),
  timeouts: Schema.optionalKey(Timeouts),
  run: Run
})
export interface Operation extends Schema.Schema.Type<typeof Operation> {}

/** The timeouts of `operation`: its manifest's, each default where it states none. */
export const boundsOf = (operation: Operation): Bounds => ({ ...DEFAULT_BOUNDS, ...operation.timeouts })

/** The executables an operation names, by the step that runs each. */
export const executablesOf = (operation: Operation): ReadonlyArray<readonly [string, string]> =>
  (["inject", "heal", "probe"] as const).flatMap((step) => {
    const file = operation.run[step]
    return file === undefined ? [] : [[step, file] as const]
  })

/** The operation manifest `file` holds as `text`, or why it is none. */
export const parse = parseWith(Operation, () => undefined)

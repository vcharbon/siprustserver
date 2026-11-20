/**
 * The judge manifest, `judges/<name>/judge.toml` (`judge.v1`, ADR-0060
 * appendix §6): what decides a verification over every cell of a scope with
 * its claims and its records. It is a TypeScript module or an executable,
 * exactly one of them.
 */
import * as Schema from "effect/Schema"
import { Name, parseWith, RelativePath, Seconds } from "./manifest-file.js"

/** The schema version a judge manifest states. */
export const SCHEMA = "judge.v1"

/** The manifest's file name in its directory. */
export const FILE = "judge.toml"

export const Judge = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  /** Its directory's name. */
  name: Name,
  /** A TypeScript module whose default export judges. */
  module: Schema.optionalKey(RelativePath),
  /** An executable that judges, answering lines on stdout. */
  run: Schema.optionalKey(RelativePath),
  timeouts: Schema.optionalKey(Schema.Struct({ judge_s: Schema.optionalKey(Seconds) }))
})
export interface Judge extends Schema.Schema.Type<typeof Judge> {}

/** The rules a shape cannot state. */
const meaningOf = (judge: Judge): string | undefined =>
  judge.module !== undefined && judge.run !== undefined
    ? "module, run: a judge is a module or an executable, not both"
    : judge.module === undefined && judge.run === undefined
    ? "module, run: a judge names its module or its executable"
    : undefined

/** The judge manifest `file` holds as `text`, or why it is none. */
export const parse = parseWith(Judge, meaningOf)

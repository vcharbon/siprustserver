/**
 * How a manifest's text becomes its value: TOML, then its schema with no key
 * beyond the ones the schema names, then the rules a shape cannot state. Every
 * failure is a {@link Refusal} naming the file, and the dotted key where one
 * is at fault, because a typo in a manifest is otherwise silence.
 */
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import { posix } from "node:path"
import * as Toml from "smol-toml"

/** Why a job does not open: the files at fault, and what is wrong with them. */
export class Refusal extends Schema.TaggedError<Refusal>()("Rig.Refusal", {
  files: Schema.Array(Schema.String),
  reason: Schema.String
}) {
  override get message(): string {
    return `${this.files.join(" and ")}: ${this.reason}`
  }
}

/** A refusal of `files` for `reason`. */
export const refuse = (files: ReadonlyArray<string>, reason: string): Refusal => new Refusal({ files, reason })

/** A name a manifest, a level or a lane states: lower-case letters, digits and dashes. */
export const Name = Schema.String.check(Schema.isPattern(/^[a-z0-9][a-z0-9-]*$/, { title: "a name of lower-case letters, digits and dashes" }))

/** Whether a relative `path` resolves above the directory it is relative to. */
const escapes = (path: string): boolean => {
  const normal = posix.normalize(path)
  return normal === ".." || normal.startsWith("../")
}

/** A non-empty string. */
export const Text = Schema.String.check(Schema.isNonEmpty())

/** A count of seconds, zero included. */
export const Seconds = Schema.Number.check(Schema.isGreaterThanOrEqualTo(0))

/**
 * A path inside the manifest's own directory, relative to it: neither absolute
 * nor, once its `..` segments are resolved, above the directory. The check is
 * on the text; a symbolic link is followed where the path is used.
 */
export const RelativePath = Text.check(
  Schema.makeFilter((path) =>
    !path.startsWith("/") && !escapes(path) || "a path inside the manifest's directory, relative to it"
  )
)

/** A parameter value a manifest states and a lane file may override. */
export const ParamValue = Schema.Union([
  Schema.String,
  Schema.Number,
  Schema.Boolean,
  Schema.Array(Schema.Union([Schema.String, Schema.Number, Schema.Boolean]))
])
export type ParamValue = typeof ParamValue.Type

/** A table of parameters by name. */
export const Params = Schema.Record(Schema.String, ParamValue)

/** A key or an array index of a schema issue's path (`["bugs"][1]["id"]`). */
const PATH_SEGMENT = /\["([^"]*)"\]|\[(\d+)\]/g

/** The path of a schema issue's `at` line as the file reads it: keys dotted, an array element by its index (`bugs[1].id`). */
const pathOf = (at: string): string =>
  [...at.matchAll(PATH_SEGMENT)]
    .map((match, i) => (match[2] !== undefined ? `[${match[2]}]` : i === 0 ? match[1] : `.${match[1]}`))
    .join("")

/**
 * One line from the schema's message: the key as its path in the file, then
 * what was wrong with it (`timeouts.open_s: Expected a value greater than or
 * equal to 0`, `bugs[1].id: Expected string`); a key the schema does not name
 * is "not a key of" `file`.
 */
export const issueLine = (message: string, file = "this file"): string => {
  const lines = message.split("\n").map((line) => line.trim()).filter((line) => line !== "")
  const at = lines.find((line) => line.startsWith("at "))
  const what = lines.find((line) => !line.startsWith("at ")) ?? message
  if (at === undefined) return what
  return `${pathOf(at)}: ${what === "Expected no excess property" ? `not a key of ${file}` : what}`
}

/** `text` as TOML, or why it is not. */
export const tomlOf = (text: string): Result.Result<unknown, string> => {
  try {
    return Result.succeed(Toml.parse(text))
  } catch (error) {
    return Result.fail(`not TOML: ${error instanceof Error ? error.message.split("\n")[0] : String(error)}`)
  }
}

/** A decoder of `schema` refusing excess keys, its failure one {@link issueLine}. */
export const decoderOf = <S extends Schema.ConstraintDecoder<unknown>>(schema: S) => {
  const decode = Schema.decodeUnknownResult(schema, { onExcessProperty: "error", errors: "first" })
  return (raw: unknown): Result.Result<S["Type"], string> => Result.mapError(decode(raw), (issue) => issueLine(issue.message))
}

/**
 * The manifest `file` holds as `text`: TOML decoded by `schema`, then
 * `meaning`, the rules its shape cannot state, which answers why the value
 * does not hold or `undefined`.
 */
export const parseWith = <S extends Schema.ConstraintDecoder<unknown>>(
  schema: S,
  meaning: (value: S["Type"]) => string | undefined
) => {
  const decode = decoderOf(schema)
  return (file: string, text: string): Result.Result<S["Type"], Refusal> => {
    const decoded = Result.flatMap(tomlOf(text), decode)
    if (Result.isFailure(decoded)) return Result.fail(refuse([file], decoded.failure))
    const why = meaning(decoded.success)
    return why === undefined ? Result.succeed(decoded.success) : Result.fail(refuse([file], why))
  }
}

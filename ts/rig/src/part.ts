/**
 * A part (ADR-0060 appendix §11): a file `targets/<kind>/<name>.toml` several
 * lanes may name, valid against the JSON Schema `parts/<kind>.schema.json` of
 * the rig path. The schema is written in the Draft 2020-12 subset Effect's
 * importer translates (`json-schema-subset.ts`): a keyword outside it refuses
 * the schema, naming its JSON Pointer. Its root is an object schema.
 *
 * The schema's root keyword `x-siprig` states what the base reads of the kind:
 *
 * - `shared`: the lanes of one job name one file of the kind, or all none;
 * - `capabilities`: per capability a part of the kind gives a lane, the fields
 *   its value must hold, each by dotted path: `true` for present, a list for
 *   one of those values. A capability stating no field is given by the part.
 *   It names no capability the base derives itself (`capability.ts`).
 */
import * as JsonSchema from "effect/JsonSchema"
import * as Result from "effect/Result"
import * as Schema from "effect/Schema"
import * as SchemaRepresentation from "effect/SchemaRepresentation"
import * as path from "node:path"
import * as Capability from "./capability.js"
import * as JsonSchemaSubset from "./json-schema-subset.js"
import { decoderOf, Name, type Refusal, refuse, tomlOf } from "./manifest-file.js"

/** The directory of the part files under a workspace or a pack's lanes. */
export const TARGETS_DIR = "targets"

/** The root keyword of a part schema the base reads. */
export const ANNOTATION = "x-siprig"

/** What a capability asks of a part's value at one dotted path: present, or one of the values. */
const Condition = Schema.Union([Schema.Literal(true), Schema.Array(Schema.Union([Schema.String, Schema.Number, Schema.Boolean]))])

const Annotation = Schema.Struct({
  shared: Schema.optionalKey(Schema.Boolean),
  capabilities: Schema.optionalKey(Schema.Record(Schema.String, Schema.Record(Schema.String, Condition)))
})

/** What the base reads of a part kind: how a file of it decodes, whether a job shares it, and the capabilities it gives. */
export interface Kind {
  readonly decode: (raw: unknown) => Result.Result<Readonly<Record<string, unknown>>, string>
  readonly shared: boolean
  readonly capabilities: ReadonlyMap<string, ReadonlyMap<string, true | ReadonlyArray<string | number | boolean>>>
}

const decodeAnnotation = decoderOf(Annotation)

/** The kind the part schema `file` states, or why the base cannot read it (`x-siprig.<key>: …`). */
export const kindOf = (file: string, schema: Readonly<Record<string, unknown>>): Result.Result<Kind, Refusal> => {
  const { [ANNOTATION]: annotation, ...document } = schema
  const read = decodeAnnotation(annotation ?? {})
  if (Result.isFailure(read)) return Result.fail(refuse([file], `${ANNOTATION}.${read.failure}`))
  const named = Object.keys(read.success.capabilities ?? {})
  const unnamed = named.find((name) => !Schema.is(Name)(name))
  if (unnamed !== undefined) {
    return Result.fail(refuse([file], `${ANNOTATION}.capabilities.${unnamed}: a capability is lower-case letters, digits and dashes`))
  }
  const derived = named.find(Capability.derivedByBase)
  if (derived !== undefined) {
    return Result.fail(refuse([file], `${ANNOTATION}.capabilities.${derived}: the base derives ${derived} itself, no part gives it`))
  }
  if (document.type !== "object") return Result.fail(refuse([file], `type: a part schema's root is "object"`))
  const unread = JsonSchemaSubset.firstUnread(document)
  if (unread !== undefined) return Result.fail(refuse([file], `${unread}: a keyword the rig does not read`))
  let decoder: (raw: unknown) => Result.Result<unknown, string>
  try {
    const imported = SchemaRepresentation.fromJsonSchemaDocument(JsonSchema.fromSchemaDraft2020_12(document), { patterns: "apply" })
    decoder = decoderOf(imported as unknown as Schema.ConstraintDecoder<unknown>)
  } catch (error) {
    return Result.fail(refuse([file], `not a JSON Schema the rig reads: ${error instanceof Error ? error.message : String(error)}`))
  }
  const capabilities = new Map(
    Object.entries(read.success.capabilities ?? {}).map(([name, fields]) => [name, new Map(Object.entries(fields))] as const)
  )
  return Result.succeed({
    decode: (raw) => decoder(raw) as Result.Result<Readonly<Record<string, unknown>>, string>,
    shared: read.success.shared === true,
    capabilities
  })
}

/** The value at the dotted `at` of `value`, or `undefined`. */
const valueAt = (value: Readonly<Record<string, unknown>>, at: string): unknown =>
  at.split(".").reduce<unknown>(
    (into, key) => (typeof into === "object" && into !== null && !Array.isArray(into) ? (into as Record<string, unknown>)[key] : undefined),
    value
  )

/** The capabilities a part of `kind` holding `value` gives the lane naming it, sorted. */
export const capabilitiesOf = (kind: Kind, value: Readonly<Record<string, unknown>>): ReadonlyArray<string> =>
  [...kind.capabilities].filter(([, fields]) =>
    [...fields].every(([at, condition]) => {
      const found = valueAt(value, at)
      return condition === true ? found !== undefined : condition.some((one) => one === found)
    })
  ).map(([name]) => name).sort()

/** A part a lane names, as read: its kind, its file's name and path, and its value. */
export interface Part {
  readonly kind: string
  readonly name: string
  readonly file: string
  readonly value: Readonly<Record<string, unknown>>
}

/** The file of the part `name` of `kind` under `root`. */
export const fileOf = (root: string, kind: string, name: string): string => path.join(root, TARGETS_DIR, kind, `${name}.toml`)

/** The part `file` holds as `text`, decoded by `kind`; a refusal names the file and the dotted field. */
export const parse = (kind: Kind, file: string, text: string): Result.Result<Readonly<Record<string, unknown>>, Refusal> =>
  Result.mapError(Result.flatMap(tomlOf(text), kind.decode), (reason) => refuse([file], reason))

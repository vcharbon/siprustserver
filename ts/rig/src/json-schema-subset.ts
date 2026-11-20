/**
 * The Draft 2020-12 subset a part schema is written in: the keywords Effect's
 * importer turns into checks, and the annotations that check nothing by their
 * definition. A keyword outside it would load and validate nothing, and so
 * would a keyword whose value has a shape it does not take (an `items` list,
 * a `maximum` string): a part schema stating either is refused (`part.ts`).
 */

/** The keywords whose value is one subschema. */
const ONE_SCHEMA = new Set(["additionalProperties", "propertyNames", "items"])

/** The keywords whose value is a list of subschemas. */
const SCHEMA_LIST = new Set(["allOf", "anyOf", "oneOf", "prefixItems"])

/** The keywords whose value maps names to subschemas. */
const SCHEMA_MAP = new Set(["properties", "patternProperties", "$defs"])

/** A data keyword's value shape, as the 2020-12 meta-schema states it: the test, and what it is in words. */
type Shape = readonly [test: (value: unknown) => boolean, is: string]

/** The seven simple types `type` names. */
const SIMPLE_TYPES = new Set(["array", "boolean", "integer", "null", "number", "object", "string"])

const isSimpleType = (value: unknown): boolean => typeof value === "string" && SIMPLE_TYPES.has(value)

const distinct = (values: ReadonlyArray<unknown>): boolean => new Set(values).size === values.length

const isString = (value: unknown): boolean => typeof value === "string"

const NUMBER: Shape = [(value) => typeof value === "number" && Number.isFinite(value), "a number"]

const COUNT: Shape = [(value) => Number.isInteger(value) && (value as number) >= 0, "a non-negative integer"]

/** The keywords the importer checks a value by, whose value is data, not a subschema, each with the shape it takes. */
const CHECKS: ReadonlyMap<string, Shape> = new Map<string, Shape>([
  ["type", [
    (value) => isSimpleType(value) || (Array.isArray(value) && value.length > 0 && value.every(isSimpleType) && distinct(value)),
    `one of ${[...SIMPLE_TYPES].join(", ")}, or a non-empty list of distinct ones`
  ]],
  ["const", [() => true, "any value"]],
  ["enum", [Array.isArray, "a list"]],
  ["$ref", [isString, "a string"]],
  ["required", [(value) => Array.isArray(value) && value.every(isString) && distinct(value), "a list of distinct strings"]],
  ["minLength", COUNT],
  ["maxLength", COUNT],
  ["pattern", [isString, "a string"]],
  ["minimum", NUMBER],
  ["maximum", NUMBER],
  ["exclusiveMinimum", NUMBER],
  ["exclusiveMaximum", NUMBER],
  ["multipleOf", [(value) => typeof value === "number" && Number.isFinite(value) && value > 0, "a number above 0"]],
  ["minProperties", COUNT],
  ["maxProperties", COUNT],
  ["minItems", COUNT],
  ["maxItems", COUNT],
  ["uniqueItems", [(value) => typeof value === "boolean", "a boolean"]]
])

/** The keywords that annotate and check nothing by their definition. */
const ANNOTATIONS = new Set(["$schema", "$id", "$comment", "title", "description", "default", "examples", "readOnly", "writeOnly"])

/** `key` as a JSON Pointer reference token (RFC 6901). */
const token = (key: string | number): string => String(key).replaceAll("~", "~0").replaceAll("/", "~1")

const isObject = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

/** Why a value stands where a subschema does and is none. */
const NO_SUBSCHEMA = "a subschema is an object or a boolean"

/**
 * The subschemas the keyword `key` at `here` states in `value`, each with its
 * JSON Pointer; why `value` is of no shape `key` takes; `[]` for a keyword
 * whose value is data of its shape.
 */
const subschemasOf = (key: string, value: unknown, here: string): ReadonlyArray<readonly [string, unknown]> | string => {
  const check = CHECKS.get(key)
  if (check !== undefined) return check[0](value) ? [] : `${here}: ${key} is ${check[1]}`
  if (ONE_SCHEMA.has(key)) return [[here, value]]
  if (SCHEMA_LIST.has(key)) {
    return Array.isArray(value) ? value.map((member, index) => [`${here}/${token(index)}`, member] as const) : `${here}: ${key} is a list of subschemas`
  }
  if (SCHEMA_MAP.has(key)) {
    return isObject(value)
      ? Object.entries(value).map(([name, member]) => [`${here}/${token(name)}`, member] as const)
      : `${here}: ${key} maps names to subschemas`
  }
  return []
}

/**
 * Why `schema`, the subschema at the JSON Pointer `at`, is outside the subset,
 * as `<pointer>: <why>` for its first fault: a keyword the importer does not
 * read, or a keyword whose value is of no shape the meta-schema gives it;
 * `undefined` where the whole schema is in it.
 */
export const firstFault = (schema: unknown, at = ""): string | undefined => {
  if (typeof schema === "boolean") return undefined
  if (!isObject(schema)) return `${at}: ${NO_SUBSCHEMA}`
  for (const [key, value] of Object.entries(schema)) {
    const here = `${at}/${token(key)}`
    if (!ONE_SCHEMA.has(key) && !SCHEMA_LIST.has(key) && !SCHEMA_MAP.has(key) && !CHECKS.has(key) && !ANNOTATIONS.has(key)) {
      return `${here}: a keyword the rig does not read`
    }
    const members = subschemasOf(key, value, here)
    if (typeof members === "string") return members
    for (const [pointer, member] of members) {
      const found = firstFault(member, pointer)
      if (found !== undefined) return found
    }
  }
  return undefined
}

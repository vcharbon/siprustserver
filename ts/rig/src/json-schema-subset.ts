/**
 * The Draft 2020-12 subset a part schema is written in: the keywords Effect's
 * importer turns into checks, and the annotations that check nothing by their
 * definition. A keyword outside it would load and validate nothing, and so
 * would a keyword stating subschemas in a shape it does not take (an `items`
 * list): a part schema stating either is refused (`part.ts`).
 */

/** The keywords whose value is one subschema. */
const ONE_SCHEMA = new Set(["additionalProperties", "propertyNames", "items"])

/** The keywords whose value is a list of subschemas. */
const SCHEMA_LIST = new Set(["allOf", "anyOf", "oneOf", "prefixItems"])

/** The keywords whose value maps names to subschemas. */
const SCHEMA_MAP = new Set(["properties", "patternProperties", "$defs"])

/** The keywords the importer checks a value by, whose value is data, not a subschema. */
const CHECKS = new Set([
  "type",
  "const",
  "enum",
  "$ref",
  "required",
  "minLength",
  "maxLength",
  "pattern",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "multipleOf",
  "minProperties",
  "maxProperties",
  "minItems",
  "maxItems",
  "uniqueItems"
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
 * whose value is data.
 */
const subschemasOf = (key: string, value: unknown, here: string): ReadonlyArray<readonly [string, unknown]> | string => {
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
 * read, or a keyword stating subschemas whose value is of no shape it takes;
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

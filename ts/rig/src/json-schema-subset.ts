/**
 * The Draft 2020-12 subset a part schema is written in: the keywords Effect's
 * importer turns into checks, and the annotations that check nothing by their
 * definition. A keyword outside it would load and validate nothing, so a part
 * schema stating one is refused (`part.ts`).
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

/** The JSON Pointer of the first keyword of `schema`, at `at`, outside the subset; `undefined` where every keyword is in it. */
export const firstUnread = (schema: unknown, at = ""): string | undefined => {
  if (!isObject(schema)) return undefined
  for (const [key, value] of Object.entries(schema)) {
    const here = `${at}/${token(key)}`
    if (ONE_SCHEMA.has(key)) {
      const found = firstUnread(value, here)
      if (found !== undefined) return found
    } else if (SCHEMA_LIST.has(key)) {
      if (!Array.isArray(value)) continue
      for (const [index, member] of value.entries()) {
        const found = firstUnread(member, `${here}/${token(index)}`)
        if (found !== undefined) return found
      }
    } else if (SCHEMA_MAP.has(key)) {
      if (!isObject(value)) continue
      for (const [name, member] of Object.entries(value)) {
        const found = firstUnread(member, `${here}/${token(name)}`)
        if (found !== undefined) return found
      }
    } else if (!CHECKS.has(key) && !ANNOTATIONS.has(key)) {
      return here
    }
  }
  return undefined
}

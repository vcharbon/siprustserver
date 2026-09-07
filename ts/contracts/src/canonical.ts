/**
 * Canonical serialization, the TypeScript half of `pivot_schema::canonical`
 * (`PCAP2TEST_PIVOT_V3.md` §2.1): keys sorted lexically at every level,
 * two-space indent, one trailing newline.
 *
 * Serialization is FORMATTER-owned. No emitter reproduces a struct's
 * declaration order by hand, and no consumer depends on one surviving a round
 * trip. Both halves state the same byte form, so a document written here and a
 * document written by the Rust formatter are byte-identical.
 *
 * `undefined` properties are dropped rather than written: an absent optional
 * key of a decoded contract must not reappear as a null.
 */

/** Serialize `value` canonically: sorted keys, two-space indent, one trailing newline. */
export const format = (value: unknown): string => `${writeValue(value, 0)}\n`

/**
 * Serialize `value` as ONE JSON Lines record: the same key order, on one line,
 * with no newline of its own — the stream owns the line breaks.
 */
export const formatLine = (value: unknown): string => writeCompact(value)

/** Canonicalize an already-parsed document's text, so a file can be checked in place. */
export const formatText = (text: string): string => format(JSON.parse(text))

/**
 * Serialize in DECLARATION order — what `serde_json::to_string_pretty` writes
 * for the e2e record kinds, which are not canonically sorted. The caller builds
 * the object with its keys in the Rust declaration order; this only adds the
 * indent and the trailing newline.
 */
export const formatDeclared = (value: unknown): string => `${JSON.stringify(value, undefined, 2)}\n`

const isPlainObject = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value)

/** Entries a canonical writer emits: sorted by key, `undefined` values dropped. */
const sortedEntries = (value: Record<string, unknown>): Array<[string, unknown]> =>
  Object.keys(value)
    .sort()
    .flatMap((key) => (value[key] === undefined ? [] : [[key, value[key]] as [string, unknown]]))

const writeScalar = (value: unknown): string => JSON.stringify(value) ?? "null"

const indent = (depth: number): string => "  ".repeat(depth)

const writeValue = (value: unknown, depth: number): string => {
  if (isPlainObject(value)) {
    const entries = sortedEntries(value)
    if (entries.length === 0) return "{}"
    const body = entries
      .map(([key, item]) => `${indent(depth + 1)}${writeScalar(key)}: ${writeValue(item, depth + 1)}`)
      .join(",\n")
    return `{\n${body}\n${indent(depth)}}`
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return "[]"
    const body = value.map((item) => `${indent(depth + 1)}${writeValue(item, depth + 1)}`).join(",\n")
    return `[\n${body}\n${indent(depth)}]`
  }
  return writeScalar(value)
}

const writeCompact = (value: unknown): string => {
  if (isPlainObject(value)) {
    const body = sortedEntries(value)
      .map(([key, item]) => `${writeScalar(key)}:${writeCompact(item)}`)
      .join(",")
    return `{${body}}`
  }
  if (Array.isArray(value)) return `[${value.map(writeCompact).join(",")}]`
  return writeScalar(value)
}

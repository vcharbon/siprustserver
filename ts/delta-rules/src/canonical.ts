/**
 * One canonical text per value a deployment's rule checks compare or key by:
 * JSON with object keys sorted, a key holding `undefined` left out as JSON
 * leaves it out, a `Map` as its entries sorted by key, a `Set` as its members
 * sorted, and `undefined` anywhere else kept as a marker. A value read back
 * from storage, its Maps and Sets restored, gives the same text, and a digest
 * of it names a probe, its case facts or a case context.
 */
import * as crypto from "node:crypto"

const UNDEFINED = "\u0000undefined"

const byKey = <V>(a: readonly [string, V], b: readonly [string, V]): number => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)

/** `value` with every Map, Set and object key in canonical order, ready for `JSON.stringify`. */
const canonicalOf = (value: unknown): unknown => {
  if (value === undefined) return UNDEFINED
  if (value === null || typeof value !== "object") return value
  if (value instanceof Map) {
    return { "\u0000map": [...value.entries()].map(([k, v]) => [String(k), canonicalOf(v)] as const).sort(byKey) }
  }
  if (value instanceof Set) return { "\u0000set": [...value].map((v) => JSON.stringify(canonicalOf(v))).sort() }
  if (Array.isArray(value)) return value.map(canonicalOf)
  return Object.fromEntries(
    Object.entries(value).filter(([, v]) => v !== undefined).map(([k, v]) => [k, canonicalOf(v)] as const).sort(byKey)
  )
}

/** The canonical text of `value`. */
export const canonicalText = (value: unknown): string => JSON.stringify(canonicalOf(value))

/** A short digest of `value`'s canonical text: 16 hex digits of its SHA-256. */
export const digestOf = (value: unknown): string =>
  crypto.createHash("sha256").update(canonicalText(value)).digest("hex").slice(0, 16)

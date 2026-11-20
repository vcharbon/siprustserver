/**
 * Header and body values read for comparison: parameter forms, media types,
 * Reason (RFC 3326), Timestamp, CSeq and RAck values, and a session
 * description's origin line.
 */
import { Fold } from "@sip/pipeline"

/** One `name=value` parameter: the name lowered and trimmed, the value trimmed and unquoted. */
const paramForm = (param: string): { readonly name: string; readonly value: string } => {
  const eq = param.indexOf("=")
  const name = (eq < 0 ? param : param.slice(0, eq)).trim().toLowerCase()
  const raw = eq < 0 ? "" : param.slice(eq + 1).trim()
  return { name, value: raw.startsWith('"') && raw.endsWith('"') ? raw.slice(1, -1) : raw }
}

/** A value's leading token and its `;` parameters in {@link paramForm}, the empty ones dropped. */
const tokenAndParams = (value: string): { readonly token: string; readonly params: ReadonlyArray<{ name: string; value: string }> } => {
  const [token = "", ...params] = Fold.topLevelSplit(value, ";").map((s) => s.trim())
  return { token, params: params.filter((p) => p.length > 0).map(paramForm) }
}

/**
 * One media-type value read for comparison: the `type/subtype` lowered, and
 * its parameters as `name=value` with the name lowered and the value
 * unquoted, sorted, the `boundary` parameter left out.
 */
export const mediaTypeApartFromBoundary = (value: string): { type: string; params: string } => {
  const { token, params } = tokenAndParams(value)
  const rest = params.filter((p) => p.name !== "boundary").map((p) => `${p.name}=${p.value}`).sort()
  return { type: token.toLowerCase(), params: rest.join(";") }
}

/** A media range's type and subtype, lowered. */
const mediaRangeOf = (item: string): { readonly type: string; readonly subtype: string } => {
  const [type = "", subtype = ""] = item.trim().toLowerCase().split("/")
  return { type, subtype }
}

/**
 * The same media ranges on both sides but the top type: at least one, every
 * `removed` one's type `application`, no `added` one's, and their subtypes
 * equal one for one.
 */
export const topTypeRenderedApplication = (removed: ReadonlyArray<string>, added: ReadonlyArray<string>): boolean => {
  if (removed.length === 0 || removed.length !== added.length) return false
  const lost = removed.map(mediaRangeOf)
  const gained = added.map(mediaRangeOf)
  const subtypes = (ranges: ReadonlyArray<{ readonly subtype: string }>) => ranges.map((r) => r.subtype).sort().join(",")
  return lost.every((r) => r.type === "application") && gained.every((r) => r.type !== "application") &&
    subtypes(lost) === subtypes(gained)
}

/**
 * One Reason item as RFC 3326 states it: the protocol, and each parameter with
 * its name lowered and its value unquoted, apart from the spacing a renderer
 * puts around `;` and `=`.
 */
export const reasonForm = (item: string): string => {
  const { token, params } = tokenAndParams(item)
  return [token.toLowerCase(), ...params.map((p) => `${p.name}=${p.value}`).sort()].join(";")
}

/** `(protocol, cause)` key of one Reason item, RFC 3326's identity for it. */
export const reasonKey = (item: string): string => {
  const [protocol = "", ...params] = item.split(";")
  const cause = params.map((x) => /^\s*cause\s*=\s*(.*)$/i.exec(x)?.[1]?.trim()).find((v) => v !== undefined)
  return `${protocol.trim().toLowerCase()};${cause ?? ""}`
}

/** A body descriptor's value read for comparison: lowered, the spacing around `;` and `=` dropped. */
export const descriptorForm = (value: string): string => value.trim().toLowerCase().replace(/\s*([;=])\s*/g, "$1")

/** The timestamp of a Timestamp value, without the delay an echo may add (RFC 3261 §20.38). */
export const timestampOf = (value: string): string => value.trim().split(/\s+/)[0] ?? ""

/** The method of a CSeq value. */
export const cseqMethodOf = (item: string): string | undefined => item.trim().split(/\s+/)[1]

/** The `rseq` and `method` thirds of an RFC 3262 §7.2 `RAck` value. */
export const rackParts = (item: string): { rseq: string; method: string } | undefined => {
  const m = /^\s*(\d+)\s+(\d+)\s+(\S+)\s*$/.exec(item)
  return m ? { rseq: m[1]!, method: m[3]! } : undefined
}

/** A description's first `o=` line, or `undefined` where it states none. */
export const originLine = (text: string): string | undefined => /^o=[^\r\n]*/m.exec(text)?.[0]

/** Two `o=` lines with six fields that differ in the sess-id and nothing else. */
export const sessIdAlone = (captured: string, replayed: string): boolean => {
  const a = captured.trim().split(/\s+/)
  const b = replayed.trim().split(/\s+/)
  return a.length === 6 && b.length === 6 && a[1] !== b[1] && a.every((f, i) => i === 1 || f === b[i])
}

/** The sess-id field of an `o=` line. */
export const sessIdOf = (line: string): string | undefined => line.trim().split(/\s+/)[1]

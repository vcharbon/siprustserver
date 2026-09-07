/**
 * How one header NAME is compared. Default transparency is a claim about
 * VALUES, not wire layout: RFC 3261 §7.3.1 makes a repeated header line and a
 * comma-appended item the same message, so each side's occurrence list is
 * flattened into items where the comma (or the header's declared separator) is
 * a separator, and the header's `Fold` says what the item list means —
 * `single` compares the occurrence list as it arrived, `ordered` compares
 * items by position, `set` by membership.
 *
 * Three properties the comparison rests on: the split is quote- and
 * angle-aware so `"Doe, John" <sip:j@h>` never tears; an empty item is
 * punctuation, never a value; and item text is compared VERBATIM apart from
 * the named RFC equivalences (name-addr shape, token+params shape) — widening
 * a comparison is how a transparency oracle goes blind.
 */
import { canonicalName } from "./wire.js"

export type Fold = "single" | "ordered" | "set"

const SET_FOLD = new Set([
  "allow",
  "allow-events",
  "supported",
  "require",
  "proxy-require",
  "unsupported",
  "accept",
  "accept-encoding",
  "accept-language",
  "content-language",
  "in-reply-to",
  "p-early-media",
  "privacy",
  "resource-priority"
])

const SINGLE_FOLD = new Set([
  "call-id",
  "cseq",
  "from",
  "to",
  "max-forwards",
  "content-length",
  "content-type",
  "expires",
  "subject",
  "user-agent",
  "server",
  "session-expires",
  "min-se",
  "rseq",
  "rack"
])

/** The fold a header name compares under. Unknown names are `ordered` — the strictest reading. */
export const foldOf = (name: string): Fold => {
  const canonical = canonicalName(name)
  if (SET_FOLD.has(canonical)) return "set"
  if (SINGLE_FOLD.has(canonical)) return "single"
  return "ordered"
}

/**
 * Names whose comma is a declared item separator. From/To are members so a
 * folded line of two addresses COUNTS as two items; they still compare as
 * `single` occurrence lists.
 */
const COMMA_LIST = new Set([
  "via",
  "contact",
  "route",
  "record-route",
  "path",
  "service-route",
  "from",
  "to",
  "refer-to",
  "referred-by",
  "reply-to",
  "diversion",
  "history-info",
  "remote-party-id",
  "p-asserted-identity",
  "p-preferred-identity",
  "reason",
  "alert-info",
  "call-info",
  "error-info",
  "content-encoding",
  "warning",
  "geolocation",
  "p-access-network-info"
])

/**
 * Names whose comma is DATA, never a separator: the credentials family carries
 * its parameters comma-separated inside one value, and a date's comma is text.
 */
const OPAQUE = new Set([
  "authorization",
  "proxy-authorization",
  "www-authenticate",
  "proxy-authenticate",
  "authentication-info",
  "date",
  "timestamp",
  "retry-after",
  "organization"
])

/** The item separator a name splits on, or `undefined` when its comma is data. */
const separatorOf = (name: string): string | undefined => {
  const canonical = canonicalName(name)
  if (OPAQUE.has(canonical)) return undefined
  if (canonical === "privacy") return ";"
  if (SET_FOLD.has(canonical) || COMMA_LIST.has(canonical)) return ","
  return undefined
}

/**
 * One value split on its top-level separators. A separator inside a quoted
 * string or inside angle brackets separates nothing.
 */
export const topLevelSplit = (value: string, separator: string): ReadonlyArray<string> => {
  const out: Array<string> = []
  let start = 0
  let quoted = false
  let angle = 0
  for (let i = 0; i < value.length; i += 1) {
    const c = value[i]
    if (quoted) {
      if (c === "\\") i += 1
      else if (c === '"') quoted = false
      continue
    }
    if (c === '"') quoted = true
    else if (c === "<") angle += 1
    else if (c === ">" && angle > 0) angle -= 1
    else if (c === separator && angle === 0) {
      out.push(value.slice(start, i))
      start = i + 1
    }
  }
  out.push(value.slice(start))
  return out
}

/**
 * A side's occurrence list flattened into its item list, whitespace-only
 * items dropped — `Allow: A, B,` states the same set as `Allow: A, B`.
 */
export const items = (name: string, values: ReadonlyArray<string>): ReadonlyArray<string> => {
  const separator = separatorOf(name)
  const flat = separator === undefined ? values : values.flatMap((v) => topLevelSplit(v, separator))
  return flat.map((v) => v.trim()).filter((v) => v.length > 0)
}

/** One parsed `name-addr` / `addr-spec`: display, URI, header parameters. */
export interface NameAddr {
  readonly display: string
  readonly uri: Uri
  readonly params: ReadonlyMap<string, string>
}

/** One parsed URI, just far enough for identity and security reads. */
export interface Uri {
  readonly scheme: string
  /** The user (or tel number), user parameters stripped; empty when user-less. */
  readonly user: string
  readonly host: string
  readonly port: number | undefined
  readonly params: ReadonlyMap<string, string>
}

const paramsOf = (parts: ReadonlyArray<string>): ReadonlyMap<string, string> => {
  const out = new Map<string, string>()
  for (const part of parts) {
    const text = part.trim()
    if (text.length === 0) continue
    const eq = text.indexOf("=")
    if (eq < 0) out.set(text.toLowerCase(), "")
    else out.set(text.slice(0, eq).trim().toLowerCase(), text.slice(eq + 1).trim())
  }
  return out
}

export const parseUri = (text: string): Uri | undefined => {
  const trimmed = text.trim()
  const colon = trimmed.indexOf(":")
  if (colon <= 0) return undefined
  const scheme = trimmed.slice(0, colon).toLowerCase()
  const rest = trimmed.slice(colon + 1)
  if (scheme === "tel") {
    const [number = "", ...params] = rest.split(";")
    return { scheme, user: number.trim(), host: "", port: undefined, params: paramsOf(params) }
  }
  if (scheme !== "sip" && scheme !== "sips") {
    return { scheme, user: rest, host: "", port: undefined, params: new Map() }
  }
  const at = rest.indexOf("@")
  const user = at < 0 ? "" : (rest.slice(0, at).split(";")[0] ?? "")
  const authorityAndParams = at < 0 ? rest : rest.slice(at + 1)
  const [authority = "", ...params] = authorityAndParams.split("?")[0]?.split(";") ?? [""]
  const hostPort = authority.trim()
  const portMatch = /^(\[[^\]]*\]|[^:]*)(?::(\d+))?$/.exec(hostPort)
  const host = (portMatch?.[1] ?? hostPort).toLowerCase()
  const port = portMatch?.[2] === undefined ? undefined : Number(portMatch[2])
  return { scheme, user, host, port, params: paramsOf(params) }
}

/** Whether a URI claims a secure transport: `sips:` or `;transport=tls`/`wss`. */
export const uriIsSecure = (uri: Uri): boolean => {
  if (uri.scheme === "sips") return true
  const transport = uri.params.get("transport")?.toLowerCase()
  return transport === "tls" || transport === "wss"
}

export const parseNameAddr = (text: string): NameAddr | undefined => {
  const trimmed = text.trim()
  const open = indexOfTopLevel(trimmed, "<")
  if (open >= 0) {
    const close = trimmed.indexOf(">", open)
    if (close < 0) return undefined
    const display = unquote(trimmed.slice(0, open).trim())
    const uri = parseUri(trimmed.slice(open + 1, close))
    if (uri === undefined) return undefined
    const after = trimmed.slice(close + 1)
    const params = paramsOf(after.split(";").slice(1))
    return { display, uri, params }
  }
  // Bare addr-spec: everything after the first `;` is a header parameter.
  const semi = trimmed.indexOf(";")
  const uri = parseUri(semi < 0 ? trimmed : trimmed.slice(0, semi))
  if (uri === undefined) return undefined
  const params = semi < 0 ? new Map<string, string>() : paramsOf(trimmed.slice(semi + 1).split(";"))
  return { display: "", uri, params }
}

const indexOfTopLevel = (text: string, needle: string): number => {
  let quoted = false
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i]
    if (quoted) {
      if (c === "\\") i += 1
      else if (c === '"') quoted = false
      continue
    }
    if (c === '"') quoted = true
    else if (c === needle) return i
  }
  return -1
}

const unquote = (text: string): string => {
  if (text.startsWith('"') && text.endsWith('"') && text.length >= 2) {
    return text.slice(1, -1).replace(/\\(.)/g, "$1")
  }
  return text
}

const mapsEqual = (a: ReadonlyMap<string, string>, b: ReadonlyMap<string, string>): boolean => {
  if (a.size !== b.size) return false
  for (const [key, value] of a) if (b.get(key) !== value) return false
  return true
}

const urisEqual = (a: Uri, b: Uri): boolean =>
  a.scheme === b.scheme &&
  a.user === b.user &&
  a.host === b.host &&
  a.port === b.port &&
  mapsEqual(a.params, b.params)

/** Names compared as parsed addresses rather than bytes. */
const NAME_ADDR_SHAPED = new Set([
  "from",
  "to",
  "contact",
  "route",
  "record-route",
  "path",
  "service-route",
  "refer-to",
  "referred-by",
  "reply-to",
  "diversion",
  "history-info",
  "remote-party-id",
  "p-asserted-identity",
  "p-preferred-identity"
])

/** Names compared as a case-insensitive token plus parsed parameters. */
const TOKEN_PARAMS_SHAPED = new Set(["p-access-network-info"])

const itemEqual = (name: string, a: string, b: string): boolean => {
  if (a === b) return true
  const canonical = canonicalName(name)
  if (NAME_ADDR_SHAPED.has(canonical)) {
    const left = parseNameAddr(a)
    const right = parseNameAddr(b)
    if (left === undefined || right === undefined) return false
    return (
      left.display === right.display &&
      urisEqual(left.uri, right.uri) &&
      mapsEqual(left.params, right.params)
    )
  }
  if (TOKEN_PARAMS_SHAPED.has(canonical)) {
    const [ta = "", ...pa] = a.split(";")
    const [tb = "", ...pb] = b.split(";")
    return ta.trim().toLowerCase() === tb.trim().toLowerCase() && mapsEqual(paramsOf(pa), paramsOf(pb))
  }
  return false
}

const orderedEqual = (name: string, a: ReadonlyArray<string>, b: ReadonlyArray<string>): boolean =>
  a.length === b.length && a.every((item, i) => itemEqual(name, item, b[i] ?? ""))

/** Whether the two sides state the same value under the name's fold. */
export const valuesEqual = (
  name: string,
  captured: ReadonlyArray<string>,
  replayed: ReadonlyArray<string>
): boolean => {
  const fold = foldOf(name)
  if (fold === "set") {
    const left = new Set(items(name, captured))
    const right = new Set(items(name, replayed))
    return left.size === right.size && [...left].every((item) => right.has(item))
  }
  if (fold === "single") {
    return orderedEqual(
      name,
      captured.map((v) => v.trim()),
      replayed.map((v) => v.trim())
    )
  }
  return orderedEqual(name, items(name, captured), items(name, replayed))
}

/**
 * The membership change of a set-folded name: which items the replay added and
 * which it removed, sorted. `undefined` for every other fold.
 */
export const setDelta = (
  name: string,
  captured: ReadonlyArray<string>,
  replayed: ReadonlyArray<string>
): { readonly added: ReadonlyArray<string>; readonly removed: ReadonlyArray<string> } | undefined => {
  if (foldOf(name) !== "set") return undefined
  const left = new Set(items(name, captured))
  const right = new Set(items(name, replayed))
  return {
    added: [...right].filter((item) => !left.has(item)).sort(),
    removed: [...left].filter((item) => !right.has(item)).sort()
  }
}

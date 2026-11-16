/**
 * Identity headers compared: name-addrs parsed, and the equalities a From, To,
 * Contact or role identity is held to, apart from the number or the tag
 * (RFC 3261 §19.1.4).
 */
import { Fold } from "@sip/pipeline"
import type { Probe } from "@sip/pipeline"
import { is, singles } from "./header-items.js"

type HeaderProbe = Probe.HeaderProbe

/** Both sides' one name-addr each, parsed. */
export const addrPair = (h: HeaderProbe): readonly [Fold.NameAddr, Fold.NameAddr] | undefined => {
  const pair = singles(h)
  if (pair === undefined) return undefined
  const a = Fold.parseNameAddr(pair[0])
  const b = Fold.parseNameAddr(pair[1])
  return a === undefined || b === undefined ? undefined : [a, b]
}

const securityDowngrade = (a: Fold.NameAddr, b: Fold.NameAddr): boolean =>
  Fold.uriIsSecure(a.uri) && !Fold.uriIsSecure(b.uri)

/** Two parameter maps equal entry for entry. */
export const sameParams = (a: ReadonlyMap<string, string>, b: ReadonlyMap<string, string>): boolean =>
  a.size === b.size && [...a].every(([k, v]) => b.get(k) === v)

/** Two name-addrs equal in every part a header states except the URI's user. */
export const sameButTheNumber = (a: Fold.NameAddr, b: Fold.NameAddr): boolean =>
  a.display === b.display &&
  a.uri.scheme === b.uri.scheme &&
  a.uri.host === b.uri.host &&
  a.uri.port === b.uri.port &&
  sameParams(a.uri.params, b.uri.params) &&
  sameParams(a.params, b.params)

/**
 * The parts of a name-addr's URI that `Fold.Uri` leaves out: the user
 * parameters (after the number, before `@`) and the `?headers`. Read from the
 * `<…>` form only; a bare addr-spec carries neither.
 */
const uriExtras = (text: string): { readonly userParams: string; readonly headers: string } => {
  const open = text.indexOf("<")
  const close = open < 0 ? -1 : text.indexOf(">", open)
  if (close < 0) return { userParams: "", headers: "" }
  const uri = text.slice(open + 1, close)
  const q = uri.indexOf("?")
  const head = q < 0 ? uri : uri.slice(0, q)
  const at = head.indexOf("@")
  const user = at < 0 ? "" : head.slice(head.indexOf(":") + 1, at)
  const semi = user.indexOf(";")
  return { userParams: semi < 0 ? "" : user.slice(semi + 1), headers: q < 0 ? "" : uri.slice(q + 1) }
}

/** A URI's `?headers` as sorted `hname=hvalue` items, the name lower-cased (§7.3.1), the value as written. */
const headerItems = (headers: string): readonly string[] =>
  headers === "" ? [] : headers.split("&").map((item) => {
    const eq = item.indexOf("=")
    return eq < 0 ? item.toLowerCase() : `${item.slice(0, eq).toLowerCase()}${item.slice(eq)}`
  }).sort()

/**
 * Two URIs' `?headers` equal as RFC 3261 §19.1.4 compares them: in any order,
 * each present in both. A value matches as written, never more leniently than
 * the §20 rule of its header.
 */
const sameUriHeaders = (a: string, b: string): boolean => {
  const x = headerItems(a)
  const y = headerItems(b)
  return x.length === y.length && x.every((item, i) => item === y[i])
}

/** An IP literal: an address of a topology, never a name a lane routes on. */
const isIpLiteral = (host: string): boolean =>
  /^\[[0-9a-f:.]+\]$/i.test(host) || /^\d{1,3}(\.\d{1,3}){3}$/.test(host)

/** Header parameters apart from the dialog tag, which the stack allocates. */
export const paramsButTag = (m: ReadonlyMap<string, string>): ReadonlyMap<string, string> =>
  new Map([...m].filter(([k]) => k !== "tag"))

/**
 * A display name that spells its own URI's user (`"5550100"
 * <sip:5550100@…>`): it states the number, so it renders with the number.
 */
const displaySpellsTheNumber = (a: Fold.NameAddr): boolean => a.display !== "" && a.display === a.uri.user

/** Display names equal, or each the spelling of its own URI's number. */
const sameDisplayButTheNumber = (a: Fold.NameAddr, b: Fold.NameAddr): boolean =>
  a.display === b.display || (displaySpellsTheNumber(a) && displaySpellsTheNumber(b))

/** User parameters with the pre-standard `phonecontext` read as RFC 3966's `phone-context`. */
const userParamsForm = (userParams: string): string => userParams.replace(/(^|;)phonecontext=/gi, "$1phone-context=")

/**
 * Two single-valued identity headers equal in every part but the number: the
 * display name (or each display spelling its own number), the URI's
 * parameters, user parameters (`phonecontext` read as `phone-context`) and
 * `?headers` (in any order), and the header parameters but the tag. The host and port are held
 * too, except that a capture host that is an IP literal may give way to the
 * lane's own (`hostMayMove`). The scheme is held unless `formMayChange`
 * (tel/sip), where the `user=phone` marker and the host belong to the form.
 */
export const identityButTheNumber = (
  h: HeaderProbe,
  opts: { readonly hostMayMove: boolean; readonly formMayChange?: boolean }
): boolean => {
  const pair = singles(h)
  const parsed = addrPair(h)
  if (pair === undefined || parsed === undefined) return false
  const [a, b] = parsed
  const form = opts.formMayChange === true
  const uriParams = (u: Fold.Uri) => (form ? new Map([...u.params].filter(([k]) => k !== "user")) : u.params)
  const hostHeld = form ||
    (a.uri.host === b.uri.host && a.uri.port === b.uri.port) ||
    (opts.hostMayMove && isIpLiteral(a.uri.host))
  const ea = uriExtras(pair[0])
  const eb = uriExtras(pair[1])
  return (
    sameDisplayButTheNumber(a, b) &&
    (form || a.uri.scheme === b.uri.scheme) &&
    hostHeld &&
    sameParams(uriParams(a.uri), uriParams(b.uri)) &&
    userParamsForm(ea.userParams) === userParamsForm(eb.userParams) &&
    sameUriHeaders(ea.headers, eb.headers) &&
    sameParams(paramsButTag(a.params), paramsButTag(b.params))
  )
}

/** The URI parameters RFC 3261 §19.1.4 never ignores when only one URI carries them. */
const NEVER_IGNORED = new Set(["user", "ttl", "method", "maddr"])

/**
 * Two URI parameter sets equal as RFC 3261 §19.1.4 compares them: one both carry
 * matches case-insensitively, one only a single URI carries is ignored but
 * `user`, `ttl`, `method` and `maddr`.
 */
const sameUriParams = (a: ReadonlyMap<string, string>, b: ReadonlyMap<string, string>): boolean =>
  [...new Set([...a.keys(), ...b.keys()])].every((name) => {
    const x = a.get(name)
    const y = b.get(name)
    if (x === undefined || y === undefined) return !NEVER_IGNORED.has(name)
    return x.toLowerCase() === y.toLowerCase()
  })

/**
 * Two name-addrs whose URIs are equal in every part, user parameters and
 * `?headers` included; the scheme, host and URI parameters compared
 * case-insensitively, the userinfo as written, a parameter one URI alone carries
 * as {@link sameUriParams} reads it, the headers as {@link sameUriHeaders} does
 * (RFC 3261 §19.1.4).
 */
export const urisEqual = (left: string, right: string): boolean => {
  const a = Fold.parseNameAddr(left)
  const b = Fold.parseNameAddr(right)
  if (a === undefined || b === undefined) return false
  const ea = uriExtras(left)
  const eb = uriExtras(right)
  return a.uri.scheme === b.uri.scheme && a.uri.user === b.uri.user && a.uri.host === b.uri.host &&
    a.uri.port === b.uri.port && sameUriParams(a.uri.params, b.uri.params) &&
    ea.userParams === eb.userParams && sameUriHeaders(ea.headers, eb.headers)
}

/** Two name-addrs equal in every part but the dialog tag. */
export const sameAddressButTag = (left: string, right: string): boolean => {
  const a = Fold.parseNameAddr(left)
  const b = Fold.parseNameAddr(right)
  if (a === undefined || b === undefined) return false
  return a.display === b.display && urisEqual(left, right) &&
    sameParams(paramsButTag(a.params), paramsButTag(b.params))
}

/** A URI user that spells a number, `+` optional. */
const spellsANumber = (user: string | undefined): boolean => /^\+?[0-9]+$/.test(user ?? "")

/**
 * Two name-addrs' URIs equal part for part as written (scheme, host, port, URI and
 * user parameters), `?headers` in any order, two users that each spell a number
 * counting as one.
 */
const sameUriAnyNumber = (left: string, right: string, a: Fold.NameAddr, b: Fold.NameAddr): boolean => {
  const ea = uriExtras(left)
  const eb = uriExtras(right)
  const sameUser = a.uri.user === b.uri.user || (spellsANumber(a.uri.user) && spellsANumber(b.uri.user))
  return a.uri.scheme === b.uri.scheme && a.uri.host === b.uri.host && a.uri.port === b.uri.port && sameUser &&
    sameParams(a.uri.params, b.uri.params) && ea.userParams === eb.userParams && sameUriHeaders(ea.headers, eb.headers)
}

/**
 * One address where the replay states it whole and the capture left a part off:
 * the same URI ({@link sameUriAnyNumber}), the capture's display name (where it
 * states one) and every header parameter but the tag kept by the replay, which
 * carries a display name or a parameter more.
 */
export const addressKeptWhole = (h: HeaderProbe): boolean => {
  const pair = singles(h)
  const parsed = addrPair(h)
  if (pair === undefined || parsed === undefined) return false
  const [a, b] = parsed
  const display = (x: Fold.NameAddr): string => x.display ?? ""
  const shown = paramsButTag(a.params)
  const kept = paramsButTag(b.params)
  const capturedKept = (display(a) === display(b) || display(a) === "") && [...shown].every(([k, v]) => kept.get(k) === v)
  const more = display(a) !== display(b) || kept.size > shown.size
  return sameUriAnyNumber(pair[0], pair[1], a, b) && capturedKept && more
}

/** Both sides' one name-addr, with URIs that differ in scheme, user, host or port. */
export const uriReplaced = (h: HeaderProbe): boolean => {
  const pair = addrPair(h)
  if (pair === undefined) return false
  const [a, b] = pair
  return a.uri.scheme !== b.uri.scheme || a.uri.user !== b.uri.user || a.uri.host !== b.uri.host || a.uri.port !== b.uri.port
}

/** A single-valued From/To/Contact on the same scheme, with no security downgrade. */
export const sameSchemeIdentity = (h: HeaderProbe): boolean => {
  if (!is(h, "from", "to", "contact")) return false
  const pair = addrPair(h)
  if (pair === undefined) return false
  const [a, b] = pair
  return !securityDowngrade(a, b) && a.uri.scheme === b.uri.scheme
}

/** A single-valued From/To carried as tel: on one side and sip: on the other, no downgrade. */
export const numberingFormSwapped = (h: HeaderProbe): boolean => {
  if (!is(h, "from", "to")) return false
  const pair = addrPair(h)
  if (pair === undefined) return false
  const [a, b] = pair
  if (securityDowngrade(a, b)) return false
  const schemes = `${a.uri.scheme}->${b.uri.scheme}`
  return schemes === "tel->sip" || schemes === "sip->tel"
}

/** A single-valued From/To on the same scheme that differs beyond the number and the topology's host. */
export const fromToRewritten = (h: HeaderProbe): boolean =>
  is(h, "from", "to") && sameSchemeIdentity(h) && !identityButTheNumber(h, { hostMayMove: true })

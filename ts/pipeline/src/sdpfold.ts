/**
 * The SDP session description read for its meaning, for the `sdp` compare
 * mode (§8.3): the reader of `v=` / `o=` / `m=` / `a=` lines the confrontation
 * keys on, the twin of the Rust `sip_message::sdp_doc` walk. `./wire.ts` reads
 * SIP; this module reads SDP and nothing else.
 *
 * Two sides compare as a session description: the session section then the
 * media sections BY POSITION (`m<i>`, 0-based, wire order); within a section
 * each line compares under its key, `<type>=` or the attribute name (`a=<name>`,
 * the four RFC 4566 §6 direction attributes under one key `a=direction`). Every
 * difference is one {@link SdpDifference} per differing key, carrying that
 * key's VERBATIM lines on both sides: the fold decides, it edits nothing.
 *
 * Two descriptions that differ in these serialization choices alone, each
 * one the RFC leaves to the writer, compare equal:
 * - the line ending (CRLF or bare LF), a blank line, whitespace trailing a
 *   line other than the `s=` and `i=` text lines, whose value runs to the end;
 * - the order of attributes of different names: RFC 4566 §5 fixes the order of
 *   every other line, not of attributes among themselves;
 * - the order of `a=rtpmap` and of `a=fmtp` lines across formats: each is bound
 *   to the format it names (RFC 4566 §6), and the `m=` line states the
 *   preference among formats;
 * - a direction left implicit or written out: a media section that states no
 *   direction attribute takes the session-level one, else `sendrecv` (RFC 4566
 *   §6, RFC 3264 §5.1), so the session-level direction compares in each media
 *   section it reaches and the session section of a description with media
 *   compares none of its own.
 * Any other choice is a row, and so is every value, the relative order of
 * same-name attributes (an attribute's own spec may give it meaning: an
 * `a=crypto` list is ordered by preference) and of one format's lines, and
 * the order of the lines RFC 4566 §5 fixes, one `order` row beside whatever
 * else differs.
 *
 * The mask is the contract between the document and the run, and it states
 * exactly what the render writes (`sip_message::rewrite_connection_and_ports`).
 * On a rebooked run `o=` sess-id and sess-version are masked (RFC 4566 §5.2
 * owner values no replay reproduces), and the expect's own `rewrite` tokens
 * name WHICH other fields are lane-owned — `c=addr`: the address of a
 * `c=IN IP4` line, an IP6 line is never written; `m=port`: the port of an `m=`
 * line where it is non-zero, a `/count` suffix kept and a port-0 stream never
 * written — and `a=rtcp` is never written, so a difference there is always the
 * system's. On a verbatim run nothing is masked: a differing `o=` is its own
 * `session:o=` row, marked where the caller read it as an origin the replayed
 * endpoint mints, as a session it breaks on a leg where the capture keeps it,
 * or as a version step of one where the capture skips (`./sdporigin.ts`).
 */
import type { Bundle } from "@sip/contracts"
import type { OriginReading } from "./sdporigin.js"

/** Which lane-owned fields the fold sets aside. */
export interface SdpMask {
  /** The `o=` sess-id and sess-version. */
  readonly origin: boolean
  /** The address of a `c=IN IP4` line. */
  readonly connectionAddress: boolean
  /** The port of an `m=` line where it is non-zero, its `/count` kept. */
  readonly mediaPort: boolean
}

/** The floor of a rebooked run: `o=` sess-id and sess-version only. */
export const FLOOR: SdpMask = { origin: true, connectionAddress: false, mediaPort: false }

/** The mask of a verbatim run: nothing masked. */
export const VERBATIM: SdpMask = { origin: false, connectionAddress: false, mediaPort: false }

/**
 * The mask an expect's `rewrite` tokens declare under the run's media mode:
 * a token names a lane-owned field, the mode says whether the lane wrote it.
 */
export const maskOf = (rewrite: ReadonlyArray<string> | undefined, media: Bundle.MediaMode): SdpMask => {
  if (media !== "rebooked") return VERBATIM
  const tokens = new Set(rewrite ?? [])
  return { ...FLOOR, connectionAddress: tokens.has("c=addr"), mediaPort: tokens.has("m=port") }
}

/** One differing line key of one section, both sides' verbatim lines in wire order. */
export interface SdpDifference {
  /** `session`, `m<i>`, or `document` where a side is no session description. */
  readonly section: string
  /**
   * The line key (`m=`, `a=rtpmap`, `a=direction`, …); `section` for a media
   * section one side lacks; `order` for a section whose lines RFC 4566 §5
   * orders stand in another order, both sides' lines whole; `sdp` for a
   * document one side is not.
   */
  readonly line: string
  readonly captured: ReadonlyArray<string>
  readonly replayed: ReadonlyArray<string>
  /** Set on a `session:o=` row whose two origins the caller read as a minted origin and its counterpart. */
  readonly mintedOrigin?: true
  /** Set on a `session:o=` row the caller read as a session the replay breaks where the capture keeps it. */
  readonly originNotContinued?: true
  /** Set on a `session:o=` row the caller read as a sess-version the capture skips where the replay steps by one. */
  readonly versionStepOne?: true
}

const DIRECTION = "a=direction"
const DIRECTIONS: ReadonlySet<string> = new Set(["sendrecv", "sendonly", "recvonly", "inactive"])

/** The direction a section that states none takes where the session states none either (RFC 3264 §5.1). */
const DEFAULT_DIRECTION = "a=sendrecv"

/** The attributes bound to the format they name, whose order across formats states nothing (RFC 4566 §6). */
const FORMAT_BOUND: ReadonlySet<string> = new Set(["a=rtpmap", "a=fmtp"])

/** The lines whose text runs to the line's end, trailing whitespace included (RFC 4566 §5.3, §5.4). */
const TEXT_LINES: ReadonlyArray<string> = ["s=", "i="]

/** The wire lines of a text, line terminators and trailing whitespace dropped but from a text line, blank lines dropped. */
const linesOf = (text: string): ReadonlyArray<string> =>
  text
    .split(/\r?\n/)
    .map((line) => (TEXT_LINES.some((type) => line.startsWith(type)) ? line : line.trimEnd()))
    .filter((line) => line !== "")

/** Whether the lines open a session description (RFC 4566 §5: `v=` first). */
const isDescription = (lines: ReadonlyArray<string>): boolean => (lines[0] ?? "").startsWith("v=")

/** The session section then each `m=`-rooted media section, wire order. */
const sectionsOf = (lines: ReadonlyArray<string>): ReadonlyArray<ReadonlyArray<string>> => {
  const out: Array<Array<string>> = [[]]
  for (const line of lines) {
    if (line.startsWith("m=")) out.push([])
    out[out.length - 1]!.push(line)
  }
  return out
}

/** The key a line compares under: `<type>=`, `a=<name>`, `b=<bwtype>`, direction attributes in any case as one. */
export const lineKey = (line: string): string => {
  const type = line.slice(0, 2)
  if (type !== "a=" && type !== "b=") return type
  const name = line.slice(2).split(":")[0] ?? ""
  if (type === "a=" && DIRECTIONS.has(name.toLowerCase())) return DIRECTION
  return `${type}${name}`
}

/**
 * The line with every masked field replaced by `*`, and only the fields the
 * render writes: the `o=` owner values under `origin`; the address of a `c=IN IP4`
 * line under `connectionAddress`; the port of an `m=` line under `mediaPort`
 * where it is a non-zero number, its `/count` kept (`6000/2` keeps the `/2`).
 */
export const maskLine = (mask: SdpMask, line: string): string => {
  if (line.startsWith("o=") && mask.origin) {
    const fields = line.slice(2).split(" ")
    for (const index of [1, 2]) if (index < fields.length) fields[index] = "*"
    return `o=${fields.join(" ")}`
  }
  if (line.startsWith("c=IN IP4 ") && mask.connectionAddress) return "c=IN IP4 *"
  if (line.startsWith("m=") && mask.mediaPort) {
    const fields = line.slice(2).split(" ")
    if (fields.length < 2) return line
    const slash = fields[1]!.indexOf("/")
    const port = slash === -1 ? fields[1]! : fields[1]!.slice(0, slash)
    if (!/^[0-9]+$/.test(port) || Number(port) === 0 || Number(port) > 65535) return line
    fields[1] = slash === -1 ? "*" : `*${fields[1]!.slice(slash)}`
    return `m=${fields.join(" ")}`
  }
  return line
}

/** What one key of a section states: its verbatim lines, and the form they compare as. */
interface Stated {
  readonly lines: ReadonlyArray<string>
  readonly form: ReadonlyArray<string>
}

/** One section read for meaning: its keys in first-appearance order, `a=direction` last, and where its RFC-ordered lines stand. */
interface Reading {
  readonly lines: ReadonlyArray<string>
  readonly keys: ReadonlyMap<string, Stated>
  /** Each non-attribute line's key, in wire order, marked where an attribute stands ahead of it. */
  readonly layout: ReadonlyArray<string>
}

/** The format a format-bound attribute line names: its first token. */
const formatOf = (line: string): string => line.slice(line.indexOf(":") + 1).trim().split(/\s+/)[0] ?? ""

/** The form a key's lines compare as: masked, in wire order, a format-bound attribute's grouped by format. */
const formOf = (mask: SdpMask, key: string, lines: ReadonlyArray<string>): ReadonlyArray<string> => {
  const masked = lines.map((line) => maskLine(mask, line))
  if (!FORMAT_BOUND.has(key)) return masked
  return masked
    .map((line, i) => ({ line, format: formatOf(line), i }))
    .sort((a, b) => (a.format < b.format ? -1 : a.format > b.format ? 1 : a.i - b.i))
    .map((entry) => entry.line)
}

/** The mark a layout entry carries where an attribute stands ahead of its line. */
const AFTER_ATTRIBUTE = "a=… "

const layoutOf = (lines: ReadonlyArray<string>): ReadonlyArray<string> => {
  let attributeSeen = false
  const out: Array<string> = []
  for (const line of lines) {
    if (line.startsWith("a=")) attributeSeen = true
    else out.push(attributeSeen ? `${AFTER_ATTRIBUTE}${lineKey(line)}` : lineKey(line))
  }
  return out
}

/** The line key a layout entry stands for, its mark dropped. */
const layoutKey = (entry: string): string => (entry.startsWith(AFTER_ATTRIBUTE) ? entry.slice(AFTER_ATTRIBUTE.length) : entry)

/**
 * One section's reading. `inherited` is the direction a section stating none
 * takes: the session section's direction lines for a media section, and
 * `undefined` for the session section of a description with media, whose
 * direction lines compare in the media sections instead.
 */
const readSection = (
  mask: SdpMask,
  lines: ReadonlyArray<string>,
  inherited: ReadonlyArray<string> | undefined
): Reading => {
  const grouped = new Map<string, Array<string>>()
  for (const line of lines) {
    const key = lineKey(line)
    const group = grouped.get(key)
    if (group === undefined) grouped.set(key, [line])
    else group.push(line)
  }
  const keys = new Map<string, Stated>()
  for (const [key, group] of grouped) {
    if (key !== DIRECTION) keys.set(key, { lines: group, form: formOf(mask, key, group) })
  }
  if (inherited !== undefined) {
    const own = grouped.get(DIRECTION)
    const stating = own ?? inherited
    keys.set(DIRECTION, { lines: stating, form: stating.length === 0 ? [DEFAULT_DIRECTION] : stating })
  }
  return { lines, keys, layout: layoutOf(lines) }
}

/** Every section of a description read for meaning, the session section first. */
const readingsOf = (mask: SdpMask, lines: ReadonlyArray<string>): ReadonlyArray<Reading> => {
  const [session = [], ...media] = sectionsOf(lines)
  const sessionDirection = session.filter((line) => lineKey(line) === DIRECTION)
  return [
    readSection(mask, session, media.length === 0 ? [] : undefined),
    ...media.map((section) => readSection(mask, section, sessionDirection))
  ]
}

const sameForm = (a: ReadonlyArray<string>, b: ReadonlyArray<string>): boolean =>
  a.length === b.length && a.every((line, i) => line === b[i])

/**
 * The text a body compares as under `sdp`: a session description is each
 * section's layout and keyed forms, sections in order; anything else
 * verbatim. Two texts fold equal exactly when {@link diffSdp} states no
 * difference between them.
 */
export const foldSdp = (mask: SdpMask, text: string): string => {
  const lines = linesOf(text)
  if (!isDescription(lines)) return text
  return readingsOf(mask, lines)
    .map((reading) => {
      const forms = [...reading.keys].map(([key, s]) => [key, s.form] as const)
      forms.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      return JSON.stringify([reading.layout, forms])
    })
    .join("\n")
}

const isOriginRow = (d: SdpDifference): boolean => d.section === "session" && d.line === "o="

/** A `session:o=` row as the caller read its origins. */
const markOrigin = (d: SdpDifference, origin: OriginReading): SdpDifference => {
  if (!isOriginRow(d)) return d
  if (origin === "minted") return { ...d, mintedOrigin: true }
  if (origin === "not-continued") return { ...d, originNotContinued: true }
  if (origin === "version-step-one") return { ...d, versionStepOne: true }
  return d
}

/**
 * Every difference between two texts read as session descriptions under the
 * mask. Where the origin is compared (a verbatim run), a `session:o=` row
 * carries the caller's reading of its origins (`origin`).
 */
export const diffSdp = (
  mask: SdpMask,
  captured: string,
  replayed: string,
  origin: OriginReading = undefined
): ReadonlyArray<SdpDifference> => {
  if (captured === replayed) return []
  const rows = diffStructure(mask, captured, replayed)
  return mask.origin ? rows : rows.map((d) => markOrigin(d, origin))
}

/** The rows: sections by position, each section's keys by their forms, then its layout. */
const diffStructure = (mask: SdpMask, captured: string, replayed: string): ReadonlyArray<SdpDifference> => {
  const left = linesOf(captured)
  const right = linesOf(replayed)
  if (!isDescription(left) || !isDescription(right)) {
    return [{ section: "document", line: "sdp", captured: [captured], replayed: [replayed] }]
  }
  const leftSections = readingsOf(mask, left)
  const rightSections = readingsOf(mask, right)
  const out: Array<SdpDifference> = []
  const count = Math.max(leftSections.length, rightSections.length)
  for (let i = 0; i < count; i++) {
    const section = i === 0 ? "session" : `m${i - 1}`
    const a = leftSections[i]
    const b = rightSections[i]
    if (a === undefined || b === undefined) {
      out.push({ section, line: "section", captured: a?.lines ?? [], replayed: b?.lines ?? [] })
      continue
    }
    out.push(...diffSection(section, a, b))
  }
  return out
}

/**
 * One difference per key whose forms differ, keys in each reading's order,
 * captured side first; then the `order` row where the lines both sides state
 * as often stand in another order, whatever else differs.
 */
const diffSection = (section: string, captured: Reading, replayed: Reading): ReadonlyArray<SdpDifference> => {
  const keys = [...new Set([...captured.keys.keys(), ...replayed.keys.keys()])]
  const none: Stated = { lines: [], form: [] }
  const out: Array<SdpDifference> = []
  for (const key of keys) {
    const a = captured.keys.get(key) ?? none
    const b = replayed.keys.get(key) ?? none
    if (!sameForm(a.form, b.form)) out.push({ section, line: key, captured: a.lines, replayed: b.lines })
  }
  if (!sameForm(sharedLayout(captured, replayed), sharedLayout(replayed, captured))) {
    out.push({ section, line: "order", captured: captured.lines, replayed: replayed.lines })
  }
  return out
}

/** `reading`'s layout kept to the keys `other` states as many times: a line one side adds or drops is its key's row, not an order. */
const sharedLayout = (reading: Reading, other: Reading): ReadonlyArray<string> => {
  const count = (layout: ReadonlyArray<string>, key: string) => layout.filter((entry) => layoutKey(entry) === key).length
  return reading.layout.filter((entry) => count(reading.layout, layoutKey(entry)) === count(other.layout, layoutKey(entry)))
}

/**
 * The SDP session description read as a document, for the `sdp` compare mode
 * (§8.3): the reader of `v=` / `o=` / `m=` / `a=` lines the confrontation keys
 * on, the twin of the Rust `sip_message::sdp_doc` walk. `./wire.ts` reads
 * SIP; this module reads SDP and nothing else.
 *
 * Two sides compare as a session description: the session section then the
 * media sections BY POSITION (`m<i>`, 0-based, wire order); within a section
 * the lines compare as a multiset keyed by line type (`<type>=`) or attribute
 * name (`a=<name>`, the four RFC 4566 §6 direction attributes under one key
 * `a=direction`). Every structural difference is one {@link SdpDifference}
 * per differing key, carrying that key's VERBATIM lines on both sides: the
 * fold decides, it edits nothing.
 *
 * The mask is the contract between the document and the run, and it states
 * exactly what the render writes (`sip_message::rewrite_connection_and_ports`).
 * `o=` sess-id and sess-version are masked always (RFC 4566 §5.2 owner values
 * no replay reproduces). The expect's own `rewrite` tokens name WHICH fields
 * are lane-owned — `c=addr`: the address of a `c=IN IP4` line, an IP6 line is
 * never written; `m=port`: the port of an `m=` line where it is non-zero, a
 * `/count` suffix kept and a port-0 stream never written — and `a=rtcp` is
 * never written, so a difference there is always the system's. The run's
 * media mode says whether the lane applied the tokens: on a `rebooked` run
 * those fields are masked and the structural rows are the whole comparison,
 * because the render re-assembles line endings there; on a `verbatim` run the
 * tokens mask nothing and the two texts must be the same BYTES — an attribute
 * reorder, a line ending, trailing whitespace or a blank line the structural
 * pass erases is then one `document:bytes` row with both texts whole.
 */
import type { Bundle } from "@sip/contracts"

/** Which lane-owned fields the fold sets aside, beside the `o=` floor it always masks, and whether the bytes must match. */
export interface SdpMask {
  /** The address of a `c=IN IP4` line. */
  readonly connectionAddress: boolean
  /** The port of an `m=` line where it is non-zero, its `/count` kept. */
  readonly mediaPort: boolean
  /** A verbatim run: once the structure matches, the two texts must be the same bytes. */
  readonly verbatim: boolean
}

/** The floor of a rebooked run: `o=` sess-id and sess-version only. */
export const FLOOR: SdpMask = { connectionAddress: false, mediaPort: false, verbatim: false }

/** The mask of a verbatim run: the floor, and the bytes must match. */
export const VERBATIM: SdpMask = { ...FLOOR, verbatim: true }

/**
 * The mask an expect's `rewrite` tokens declare under the run's media mode:
 * a token names a lane-owned field, the mode says whether the lane wrote it.
 */
export const maskOf = (rewrite: ReadonlyArray<string> | undefined, media: Bundle.MediaMode): SdpMask => {
  if (media !== "rebooked") return VERBATIM
  const tokens = new Set(rewrite ?? [])
  return { connectionAddress: tokens.has("c=addr"), mediaPort: tokens.has("m=port"), verbatim: false }
}

/** One differing line key of one section, both sides' verbatim lines in wire order. */
export interface SdpDifference {
  /** `session`, `m<i>`, or `document` where a side is no session description. */
  readonly section: string
  /**
   * The line key (`m=`, `a=rtpmap`, …); `section` for a media section one side
   * lacks; `sdp` for a document one side is not; `bytes` for two descriptions
   * a verbatim run carried as different bytes.
   */
  readonly line: string
  readonly captured: ReadonlyArray<string>
  readonly replayed: ReadonlyArray<string>
}

const DIRECTIONS: ReadonlySet<string> = new Set(["sendrecv", "sendonly", "recvonly", "inactive"])

/** The wire lines of a text, line terminators and trailing whitespace dropped, blank lines dropped. */
const linesOf = (text: string): ReadonlyArray<string> =>
  text
    .split(/\r?\n/)
    .map((line) => line.trimEnd())
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

/** The key a line compares under: `<type>=`, `a=<name>`, `b=<bwtype>`, direction attributes as one. */
export const lineKey = (line: string): string => {
  const type = line.slice(0, 2)
  if (type !== "a=" && type !== "b=") return type
  const name = line.slice(2).split(":")[0] ?? ""
  if (type === "a=" && DIRECTIONS.has(name)) return "a=direction"
  return `${type}${name}`
}

/**
 * The line with every masked field replaced by `*`, and only the fields the
 * render writes: the `o=` owner values always; the address of a `c=IN IP4`
 * line under `connectionAddress`; the port of an `m=` line under `mediaPort`
 * where it is a non-zero number, its `/count` kept (`6000/2` keeps the `/2`).
 */
export const maskLine = (mask: SdpMask, line: string): string => {
  if (line.startsWith("o=")) {
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

const sortedMasked = (mask: SdpMask, lines: ReadonlyArray<string>): ReadonlyArray<string> =>
  lines.map((line) => maskLine(mask, line)).sort()

/**
 * The text a body compares as under `sdp`. On a verbatim run it is the text
 * itself: the bytes must match. On a rebooked run a session description is
 * its sections in order, each section its masked lines sorted, sections
 * joined by a blank line; anything else verbatim. Two texts fold equal
 * exactly when {@link diffSdp} states no difference between them.
 */
export const foldSdp = (mask: SdpMask, text: string): string => {
  if (mask.verbatim) return text
  return structuralFold(mask, text)
}

const structuralFold = (mask: SdpMask, text: string): string => {
  const lines = linesOf(text)
  if (!isDescription(lines)) return text
  return sectionsOf(lines)
    .map((section) => sortedMasked(mask, section).join("\n"))
    .join("\n\n")
}

/**
 * Every difference between two texts read as session descriptions under the
 * mask: the structural rows, or — on a verbatim run whose structure matches
 * while the bytes do not — the one `document:bytes` row.
 */
export const diffSdp = (mask: SdpMask, captured: string, replayed: string): ReadonlyArray<SdpDifference> => {
  if (captured === replayed) return []
  const structural = diffStructure(mask, captured, replayed)
  if (structural.length > 0 || !mask.verbatim) return structural
  return [{ section: "document", line: "bytes", captured: [captured], replayed: [replayed] }]
}

/** The structural rows: sections by position, lines per section as a masked multiset. */
const diffStructure = (mask: SdpMask, captured: string, replayed: string): ReadonlyArray<SdpDifference> => {
  const left = linesOf(captured)
  const right = linesOf(replayed)
  if (!isDescription(left) || !isDescription(right)) {
    return [{ section: "document", line: "sdp", captured: [captured], replayed: [replayed] }]
  }
  const leftSections = sectionsOf(left)
  const rightSections = sectionsOf(right)
  const out: Array<SdpDifference> = []
  const count = Math.max(leftSections.length, rightSections.length)
  for (let i = 0; i < count; i++) {
    const section = i === 0 ? "session" : `m${i - 1}`
    const a = leftSections[i]
    const b = rightSections[i]
    if (a === undefined || b === undefined) {
      out.push({ section, line: "section", captured: a ?? [], replayed: b ?? [] })
      continue
    }
    out.push(...diffSection(mask, section, a, b))
  }
  return out
}

/** One difference per key whose masked multisets differ, keys in first-appearance order, captured side first. */
const diffSection = (
  mask: SdpMask,
  section: string,
  captured: ReadonlyArray<string>,
  replayed: ReadonlyArray<string>
): ReadonlyArray<SdpDifference> => {
  const keys = [...new Set([...captured, ...replayed].map(lineKey))]
  const out: Array<SdpDifference> = []
  for (const key of keys) {
    const a = captured.filter((line) => lineKey(line) === key)
    const b = replayed.filter((line) => lineKey(line) === key)
    const folded = sortedMasked(mask, b)
    const same = a.length === b.length && sortedMasked(mask, a).every((line, i) => line === folded[i])
    if (!same) out.push({ section, line: key, captured: a, replayed: b })
  }
  return out
}

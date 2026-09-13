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
 * `a=direction`), so attribute order is erased and nothing else is. Every
 * difference is one {@link SdpDifference} per differing key, carrying that
 * key's VERBATIM lines on both sides: the fold decides, it edits nothing.
 *
 * The mask is the contract between the document and the run. `o=` sess-id and
 * sess-version are masked always (RFC 4566 §5.2 owner values no replay
 * reproduces). The expect's own `rewrite` tokens name WHICH fields are
 * lane-owned — `c=addr`: the `c=` address and the `a=rtcp` address (RFC 3605);
 * `m=port`: the `m=` port and the `a=rtcp` port — and the run's media mode
 * says whether the lane applied them: on a `rebooked` run those fields are
 * masked, on a `verbatim` run the tokens mask nothing, so a `c=` or an `m=`
 * the system alters is a difference. A token the fold does not list masks
 * nothing.
 */
import type { Bundle } from "@sip/contracts"

/** Which lane-owned fields the fold sets aside, beside the `o=` floor it always masks. */
export interface SdpMask {
  /** The `c=` address and the `a=rtcp` address. */
  readonly connectionAddress: boolean
  /** The `m=` port and the `a=rtcp` port. */
  readonly mediaPort: boolean
}

/** The floor: `o=` sess-id and sess-version only. */
export const FLOOR: SdpMask = { connectionAddress: false, mediaPort: false }

/**
 * The mask an expect's `rewrite` tokens declare under the run's media mode:
 * a token names a lane-owned field, the mode says whether the lane wrote it.
 */
export const maskOf = (rewrite: ReadonlyArray<string> | undefined, media: Bundle.MediaMode): SdpMask => {
  if (media !== "rebooked") return FLOOR
  const tokens = new Set(rewrite ?? [])
  return { connectionAddress: tokens.has("c=addr"), mediaPort: tokens.has("m=port") }
}

/** One differing line key of one section, both sides' verbatim lines in wire order. */
export interface SdpDifference {
  /** `session`, `m<i>`, or `document` where a side is no session description. */
  readonly section: string
  /** The line key (`m=`, `a=rtpmap`, …), `section` for a media section one side lacks, `sdp` for the document. */
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

/** The line with every masked field replaced by `*`. */
export const maskLine = (mask: SdpMask, line: string): string => {
  const fields = line.slice(2).split(" ")
  const star = (index: number) => {
    if (index < fields.length) fields[index] = "*"
  }
  if (line.startsWith("o=")) {
    star(1)
    star(2)
  } else if (line.startsWith("c=") && mask.connectionAddress) {
    star(2)
  } else if (line.startsWith("m=") && mask.mediaPort) {
    star(1)
  } else if (line.startsWith("a=rtcp:")) {
    // RFC 3605: `a=rtcp:<port> [<nettype> <addrtype> <address>]`.
    if (mask.mediaPort) fields[0] = "rtcp:*"
    if (mask.connectionAddress) star(3)
  } else {
    return line
  }
  return `${line.slice(0, 2)}${fields.join(" ")}`
}

const sortedMasked = (mask: SdpMask, lines: ReadonlyArray<string>): ReadonlyArray<string> =>
  lines.map((line) => maskLine(mask, line)).sort()

/**
 * The text a body compares as under `sdp`: a session description as its
 * sections in order, each section its masked lines sorted, sections joined by
 * a blank line; anything else verbatim. Two texts fold equal exactly when
 * {@link diffSdp} states no difference between them.
 */
export const foldSdp = (mask: SdpMask, text: string): string => {
  const lines = linesOf(text)
  if (!isDescription(lines)) return text
  return sectionsOf(lines)
    .map((section) => sortedMasked(mask, section).join("\n"))
    .join("\n\n")
}

/** Every difference between two texts read as session descriptions under the mask. */
export const diffSdp = (mask: SdpMask, captured: string, replayed: string): ReadonlyArray<SdpDifference> => {
  if (captured === replayed) return []
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

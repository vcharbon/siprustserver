/**
 * The attribute order a stack writes when it serializes a session description
 * itself: the order rules of the Rust `sip_message::canonical_form`, and only
 * those. The shared fixture `crates/sip-message/tests/fixtures/sdp-form-order.txt`
 * holds the two alike.
 *
 * The session section stays as written. Each media section is its `m=` line,
 * its other non-attribute lines as written, each format's `a=rtpmap` then its
 * `a=fmtp` in the `m=` format order (one format's lines in wire order), the
 * other attributes as written, then the direction attributes (RFC 3264 §6.1).
 *
 * Unlike the Rust form this twin only moves lines: it adds no direction, keeps
 * each line terminator at its position and a blank line as a non-attribute
 * line, so two texts that differ in anything but order still differ in it.
 */

const DIRECTIONS: ReadonlySet<string> = new Set(["sendrecv", "sendonly", "recvonly", "inactive"])

/** Whether an attribute line is one of the four RFC 4566 §6 direction attributes. */
const isDirection = (line: string): boolean => line.startsWith("a=") && DIRECTIONS.has(line.slice(2).trim().toLowerCase())

/** The format a `prefix` attribute (`a=rtpmap:` / `a=fmtp:`) names; undefined for another attribute. */
const formatOf = (line: string, prefix: string): string | undefined => {
  if (!line.startsWith(prefix)) return undefined
  const rest = line.slice(prefix.length).trim()
  return rest === "" ? undefined : rest.split(/\s+/)[0]
}

/** One media section's lines in form: `m=`, the other non-attribute lines, the format attributes by format, the other attributes, the directions. */
const mediaInForm = (section: ReadonlyArray<string>): ReadonlyArray<string> => {
  const [m, ...rest] = section
  if (m === undefined) return section
  const formats = m.slice(2).trim().split(/\s+/).slice(3)
  const attributes = rest.filter((line) => line.startsWith("a="))
  const others = rest.filter((line) => !line.startsWith("a="))
  const bound = new Map<string, { readonly rtpmaps: Array<number>; readonly fmtps: Array<number> }>()
  const boundTo = (format: string) => {
    const found = bound.get(format)
    if (found !== undefined) return found
    const fresh = { rtpmaps: [], fmtps: [] }
    bound.set(format, fresh)
    return fresh
  }
  attributes.forEach((line, i) => {
    const rtpmap = formatOf(line, "a=rtpmap:")
    if (rtpmap !== undefined) boundTo(rtpmap).rtpmaps.push(i)
    const fmtp = formatOf(line, "a=fmtp:")
    if (fmtp !== undefined) boundTo(fmtp).fmtps.push(i)
  })
  const taken = attributes.map(() => false)
  const out = [m, ...others]
  for (const format of formats) {
    const lines = bound.get(format)
    if (lines === undefined) continue
    bound.delete(format)
    for (const i of [...lines.rtpmaps, ...lines.fmtps]) {
      taken[i] = true
      out.push(attributes[i]!)
    }
  }
  out.push(...attributes.filter((line, i) => !taken[i] && !isDirection(line)))
  out.push(...attributes.filter(isDirection))
  return out
}

/**
 * `text` with each media section's lines in the stack's attribute order; the
 * text as written where it is no session description (no `v=` first) or
 * states no media section.
 */
export const attributeOrder = (text: string): string => {
  if (!text.startsWith("v=")) return text
  // Lines at even indexes, the terminator after each at the odd ones.
  const parts = text.split(/(\r?\n)/)
  const lines = parts.filter((_, i) => i % 2 === 0)
  const terminators = parts.filter((_, i) => i % 2 === 1)
  // A text ending in a terminator leaves an empty tail that is no line.
  if (lines[lines.length - 1] === "") lines.pop()
  const starts = lines.flatMap((line, i) => (line.startsWith("m=") ? [i] : []))
  if (starts.length === 0) return text
  const inForm = lines.slice(0, starts[0])
  starts.forEach((start, n) => inForm.push(...mediaInForm(lines.slice(start, starts[n + 1] ?? lines.length))))
  return inForm.map((line, i) => line + (terminators[i] ?? "")).join("")
}

/**
 * A header probe's two sides as items: access by canonical name, the items
 * each side carries as the header's grammar splits them, their set delta, and
 * what a Content-Length and a session description's descriptors state.
 */
import { Fold, Wire } from "@sip/pipeline"
import type { Probe } from "@sip/pipeline"

type ProbeValue = Probe.Probe
type HeaderProbe = Probe.HeaderProbe

/** A side of a confrontation. */
export type Side = "captured" | "replayed"

/** A header name in its canonical form. */
export const canon = Wire.canonicalName

/** Whether `p` is a header probe of one of `names` (canonical). */
export const is = (p: ProbeValue, ...names: ReadonlyArray<string>): boolean =>
  p.kind === "header" && names.includes(canon(p.name))

/** One side's items, split as the header's grammar splits them, each trimmed. */
export const itemsOf = (h: HeaderProbe, side: Side): ReadonlyArray<string> => Fold.items(h.name, h[side])

/** One side's one item, where it carries exactly one. */
export const single = (h: HeaderProbe, side: Side): string | undefined => {
  const list = itemsOf(h, side)
  return list.length === 1 ? list[0] : undefined
}

/** Both sides' one item each, where each carries exactly one. */
export const singles = (h: HeaderProbe): readonly [string, string] | undefined => {
  const left = single(h, "captured")
  const right = single(h, "replayed")
  return left === undefined || right === undefined ? undefined : [left, right]
}

/** The capture carries items and the replay none. */
export const onlyCaptured = (h: HeaderProbe): boolean =>
  itemsOf(h, "captured").length > 0 && itemsOf(h, "replayed").length === 0

/** The replay carries items and the capture none. */
export const onlyReplayed = (h: HeaderProbe): boolean =>
  itemsOf(h, "captured").length === 0 && itemsOf(h, "replayed").length > 0

/** Both sides carry items. */
export const bothSides = (h: HeaderProbe): boolean =>
  itemsOf(h, "captured").length > 0 && itemsOf(h, "replayed").length > 0

/** Captured and replayed carry the same item count, at least one, and still differ: a rendering. */
export const valueDiffers = (h: HeaderProbe): boolean => {
  const captured = itemsOf(h, "captured")
  return captured.length > 0 && captured.length === itemsOf(h, "replayed").length
}

/** The set delta of a probe's two sides, as the header's grammar reads a set. */
export const delta = (h: HeaderProbe) => Fold.setDelta(h.name, h.captured, h.replayed)

/** A set the replay narrowed: items removed. */
export const itemsRemoved = (h: HeaderProbe): boolean => {
  const d = delta(h)
  return d !== undefined && d.removed.length > 0
}

/** The one item a set lost where the replay removed that item alone and added nothing. */
export const removedAlone = (h: HeaderProbe): string | undefined => {
  const d = delta(h)
  return d !== undefined && d.added.length === 0 && d.removed.length === 1 ? d.removed[0] : undefined
}

/**
 * Whether a side's Content-Length states a body: a non-zero length; an absent
 * length is 0; a length that is no single number answers `undefined`.
 */
export const lengthStatesBody = (h: HeaderProbe, side: Side): boolean | undefined => {
  const values = itemsOf(h, side)
  if (values.length === 0) return false
  const value = values.length === 1 ? values[0]!.trim() : ""
  return /^[0-9]+$/.test(value) ? Number(value) > 0 : undefined
}

/**
 * A descriptor of an SDP body the capture carried and the replay dropped: a
 * non-zero Content-Length where the replay states 0 or none, or the capture's one
 * Content-Type `application/sdp` or Content-Disposition `session` or
 * `early-session` where the replay carries none.
 */
export const sdpDescriptorDropped = (h: HeaderProbe): boolean => {
  const captured = itemsOf(h, "captured")
  const replayed = itemsOf(h, "replayed")
  if (is(h, "content-length")) return replayed.every((v) => v.trim() === "0") && captured.some((v) => v.trim() !== "0")
  if (replayed.length > 0 || captured.length !== 1) return false
  const token = captured[0]!.split(";")[0]!.trim().toLowerCase()
  return is(h, "content-type") ? token === "application/sdp" : ["session", "early-session"].includes(token)
}

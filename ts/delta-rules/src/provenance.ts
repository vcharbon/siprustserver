/**
 * Where a header probe's items came from: what the capture shows the system
 * being handed, what the run drove in, and the relay and mint readings built
 * on them.
 */
import { Fold } from "@sip/pipeline"
import type { Probe } from "@sip/pipeline"
import { delta, itemsOf, onlyCaptured } from "./header-items.js"
import { isRequest, statusOf } from "./scope.js"

type HeaderProbe = Probe.HeaderProbe

/** Every item the capture shows the system being handed, split as the header's grammar splits it. */
export const inboundItems = (h: HeaderProbe): ReadonlySet<string> =>
  new Set([...h.inboundValues.map((v) => v.trim()), ...Fold.items(h.name, h.inboundValues)])

/** Every one of `items` (trimmed) is one the capture shows the system being handed. */
export const allHanded = (h: HeaderProbe, items: ReadonlyArray<string>): boolean => {
  const handed = inboundItems(h)
  return items.every((item) => handed.has(item.trim()))
}

/** Every one of `items` is byte-equal to a whole inbound value. */
export const amongInboundValues = (h: HeaderProbe, items: ReadonlyArray<string>): boolean =>
  items.every((item) => h.inboundValues.includes(item))

/**
 * The run handed the system this header for THIS message, or handed it no input
 * at all. A relay claim needs it: an `auto` step sends identifying headers only,
 * so the capture's inbound evidence says nothing about what the system received.
 */
export const drivenIn = (h: HeaderProbe): boolean => h.driven !== false

/**
 * {@link drivenIn}, less the one message an unidentified input never licenses a
 * relay claim on: an originated INVITE always relays the caller's, so `driven`
 * undefined there is the confronter's reach, not a mint.
 */
export const relayInputHeld = (h: HeaderProbe): boolean =>
  h.scope.kind === "initial-invite" ? h.driven === true : drivenIn(h)

/**
 * The items the capture carried and the replay lost: every captured item of a
 * header the replay omits, or the removed items of a set the replay narrowed.
 */
const lostItems = (h: HeaderProbe): ReadonlyArray<string> | undefined => {
  if (itemsOf(h, "captured").length === 0) return undefined
  const d = delta(h)
  if (d === undefined) return itemsOf(h, "replayed").length === 0 ? itemsOf(h, "captured") : undefined
  return d.removed.length > 0 ? d.removed : undefined
}

/** Every item the replay lost is one the capture shows the system being handed. */
export const lostWereHanded = (h: HeaderProbe): boolean => allHanded(h, lostItems(h) ?? [])

/** A header the capture shows reaching the system, the run never drove in, and the replay lost items of. */
export const undrivenLoss = (h: HeaderProbe): boolean =>
  h.driven === false && h.inbound && lostItems(h) !== undefined

/** {@link undrivenLoss} on the initial INVITE the run identified no input for. */
export const unidentifiedLoss = (h: HeaderProbe): boolean =>
  h.driven === undefined && h.inbound && h.scope.kind === "initial-invite" && lostItems(h) !== undefined

/** The relay dropped a header the capture shows reaching the system, driven in by the run. */
export const notRelayed = (h: HeaderProbe): boolean => onlyCaptured(h) && h.inbound && drivenIn(h)

/** The captured system minted a header it was never handed, and the replay does not. */
export const notGenerated = (h: HeaderProbe): boolean => onlyCaptured(h) && !h.inbound

/**
 * A header the capture shows the system relaying on an ACK, driven in by the
 * run, and absent from the replay: the families an ACK composed from dialog
 * state drops.
 */
export const droppedFromAck = (h: HeaderProbe): boolean =>
  onlyCaptured(h) && isRequest(h.scope, "ACK") && h.inbound && drivenIn(h)

/** A header the capture carried on a 491 to INVITE and the replay's 491 lacks. */
export const glareRejectionLost = (h: HeaderProbe): boolean =>
  statusOf(h.scope, "INVITE") === 491 && h.captured.length > 0 && h.replayed.length === 0

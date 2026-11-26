/**
 * What of a captured message survives a relay and ties the relayed copy to it.
 *
 * A B2BUA relaying a response re-mints every tier-1 field — tags, Via, Call-ID,
 * CSeq number — so none of them pairs the two halves of a relay. The body it
 * carries does: a relay passes a session description on with its origin line
 * (RFC 4566 §5.2 `o=`, globally unique per session and version) intact, and a
 * bare message stays bare. Two emissions of one status are told apart by that,
 * where capture time alone pairs an arrival with whichever came last. Where a
 * deployment names them, header values only the far side holds tie a bare
 * relay to its source the same way (`RelayMarker`).
 */
import { body, canonicalName, headersInOrder, type LaidOut } from "./wire.js"
import { Wire } from "@sip/contracts"

/**
 * The value of a header that ties a relayed copy to its source, where the
 * deployment names one: a value only the far side of the call holds and a
 * relay passes on unchanged, such as a correlation identifier the far party
 * minted. Undefined for any other header, and for a token any platform may
 * state on its own (a capability, a privacy setting), which ties nothing.
 */
export type RelayMarker = (name: string, value: string) => string | undefined

/** What an unconfigured pipeline runs: no header ties a relay to its source. */
export const NO_RELAY_MARKS: RelayMarker = () => undefined

/** The identifying values a message carries, each `<canonical name>:<value>`, once each. */
export const relayMarksOf = (m: Wire.Msg, marker: RelayMarker): ReadonlyArray<string> => {
  const marks = new Set<string>()
  for (const h of headersInOrder(m)) {
    const value = marker(h.name, h.value.trim())
    if (value !== undefined && value !== "") marks.add(`${canonicalName(h.name)}:${value}`)
  }
  return [...marks]
}

/** The `o=` line a description states, verbatim; undefined where it states none. */
const originLine = (text: string): string | undefined => /^o=[^\r\n]*/m.exec(text)?.[0]?.trimEnd()

/**
 * The relay image of a message: `bare` when it carries no body, `sdp:<o= line>`
 * when its body states a session origin, `body` for any other body. Equal
 * images on two legs are the same content; a body without an origin line
 * identifies nothing beyond its presence.
 */
export const relayImageOf = (m: LaidOut): string => {
  const payload = body(m)
  if (payload === undefined) return "bare"
  const origin = originLine(Wire.latin1Of(payload.bytes))
  return origin === undefined ? "body" : `sdp:${origin}`
}

/** Whether an image names one session description, the only content a skewed capture stamp may pair on. */
export const identifiesSession = (image: string | undefined): boolean => image?.startsWith("sdp:") === true

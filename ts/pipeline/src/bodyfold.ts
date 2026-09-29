/**
 * How an expected BODY is compared with the one received, under the
 * `compare` mode the expectation states (§8.3). Both sides go through the
 * same fold, so a difference the fold erases is erased on both and a
 * difference it keeps is stated verbatim.
 *
 * `exact` is identity. `xml` erases exactly three things and nothing else:
 * the XML declaration is dropped, whitespace-only text between two tags is
 * removed wherever it sits (mixed content included), and the ends are
 * trimmed. Attributes keep their order and entities stay as written. `sdp`
 * reads both sides as a session description under a mask (`./sdpfold.ts`),
 * the one fold that takes a parameter: the mask is the document's and the
 * run's, never the fold's own, and on a verbatim run it is identity. Widening
 * a fold is how a body oracle goes blind, so what it does not list, it does
 * not do.
 */
import type { Body } from "@sip/contracts"
import { FLOOR, foldSdp, type SdpMask } from "./sdpfold.js"

/** The text a body compares under `compare`; `mask` reads only under `sdp`. */
export const foldBody = (
  compare: Body.BodyCompare | undefined,
  text: string,
  mask: SdpMask = FLOOR
): string => {
  switch (compare ?? "exact") {
    case "exact":
      return text
    case "xml":
      return text
        .replace(/^\s*<\?xml\b[^>]*\?>/, "")
        .replace(/>\s+</g, "><")
        .trim()
    case "sdp":
      return foldSdp(mask, text)
  }
}

/** Whether two bodies are the same under `compare`. */
export const bodiesEqual = (
  compare: Body.BodyCompare | undefined,
  captured: string,
  replayed: string,
  mask: SdpMask = FLOOR
): boolean => foldBody(compare, captured, mask) === foldBody(compare, replayed, mask)

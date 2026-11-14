/**
 * The case the re-INVITE capture records (`README.md`, "The recorded
 * samples"): the basic call (`answered-call.ts`), in which Alice Example puts
 * the answered call on hold with a re-INVITE, a sendonly offer that Bob
 * Example answers recvonly (RFC 3264 §8.4), before she hangs up.
 */
import { cellOf } from "./answered-call.ts"

/** The case's title, and the cell's name. */
export const TITLE = "alice example holds her call to bob example with a re-invite"

export default [cellOf({ title: TITLE, note: "an answered call held by the caller's re-INVITE, then ended by her BYE", holds: true })]

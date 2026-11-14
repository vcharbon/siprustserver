/**
 * The case the basic call capture records (`README.md`, "The recorded
 * samples"): Alice Example calls Bob Example, who rings and answers, and
 * Alice hangs up (`answered-call.ts`).
 */
import { cellOf } from "./answered-call.ts"

/** The case's title, and the cell's name. */
export const TITLE = "alice example calls bob example, who answers"

export default [cellOf({ title: TITLE, note: "an answered call ended by the caller's BYE" })]

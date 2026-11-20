/**
 * The case the emergency call capture records (`README.md`, "The recorded
 * samples"): the basic call (`answered-call.ts`), its INVITE marked an
 * emergency call by `Resource-Priority: esnet.0` (RFC 4412 §3.1, the `esnet`
 * namespace of RFC 7135), which the system carries to the callee side.
 */
import { cellOf } from "./answered-call.ts"

/** The case's title, and the cell's name. */
export const TITLE = "alice example places an emergency call to bob example"

/** The header marking the INVITE an emergency call. */
export const RESOURCE_PRIORITY = { name: "Resource-Priority", value: "esnet.0" } as const

export default [
  cellOf({
    title: TITLE,
    note: "an answered call whose INVITE carries an emergency Resource-Priority",
    inviteHeaders: [RESOURCE_PRIORITY]
  })
]

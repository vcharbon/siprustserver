/**
 * A flows document read off a file, leg by leg.
 *
 * The envelope — the document with its `legs` emptied — is parsed as one small
 * value and each leg apart, then the two are put back together, so a capture of
 * thousands of calls is never held as one string: V8 refuses one above 512 MiB.
 * Lenient like the contract it builds (`Flows`): nothing re-emits a flows
 * document, so an emitter that added a field must not break this reader.
 */
import type { Flows } from "@sip/contracts"
import { Buffer } from "node:buffer"
import { segments } from "./flows-segments.js"

/**
 * The flows document at `file`. The envelope's two halves meet around an empty
 * `legs` array — the head ends on its `[` and the tail opens on its `]` — which
 * is why they parse as one value with the legs taken out.
 */
export const readFlowsFile = (file: string): Flows.FlowsDoc => {
  const legs: Array<Flows.Leg> = []
  let head = ""
  let tail = ""
  for (const segment of segments(file)) {
    if (segment.kind === "head") head = Buffer.from(segment.bytes).toString("utf8")
    else if (segment.kind === "tail") tail = Buffer.from(segment.bytes).toString("utf8")
    else {
      legs.push(
        JSON.parse(Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")) as Flows.Leg
      )
    }
  }
  return { ...(JSON.parse(head + tail) as Flows.FlowsDoc), legs }
}

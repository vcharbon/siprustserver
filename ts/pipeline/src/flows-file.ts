/**
 * A flows document read off a file, leg by leg.
 *
 * The envelope — the document with its `legs` emptied — is parsed as one small
 * value and each leg apart, then the two are put back together, so a capture of
 * thousands of calls is never held as one string: V8 refuses one above 512 MiB.
 * Lenient like the contract it builds (`Flows`): nothing re-emits a flows
 * document, so an emitter that added a field must not break this reader.
 */
import { Flows } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { Buffer } from "node:buffer"
import { segments } from "./flows-segments.js"

/** A leg the `Flows` contract refuses, named by the index the document uses for it. */
export class FlowsLegRefused extends Schema.TaggedError<FlowsLegRefused>()(
  "Pipeline.FlowsLegRefused",
  { file: Schema.String, index: Schema.Int, reason: Schema.String }
) {
  override get message(): string {
    return `${this.file}: legs[${this.index}] is not a leg this contract models: ${this.reason}`
  }
}

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

const decodeLeg = Schema.decodeUnknownEffect(Flows.Leg)

/**
 * The flows document at `file`, DECODED: the envelope against `Flows.FlowsDoc`
 * with its legs taken out, each leg against `Flows.Leg` apart, then the two put
 * back together — the document `Flows.parseFlows` builds, off a file no reader
 * holds whole. The schema VERSION is the caller's rule, as it is for a decoded
 * value from anywhere else.
 */
export const readFlowsFileDecoded = Effect.fn("FlowsFile.readFlowsFileDecoded")(function* (
  file: string
) {
  const legs: Array<Flows.Leg> = []
  let head = ""
  let tail = ""
  for (const segment of segments(file)) {
    if (segment.kind === "head") head = Buffer.from(segment.bytes).toString("utf8")
    else if (segment.kind === "tail") tail = Buffer.from(segment.bytes).toString("utf8")
    else {
      const text = Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")
      legs.push(
        yield* decodeLeg(JSON.parse(text) as unknown).pipe(
          Effect.mapError((cause) =>
            new FlowsLegRefused({ file, index: segment.index, reason: cause.message })
          )
        )
      )
    }
  }
  const envelope = yield* Flows.decodeFlows({
    ...(JSON.parse(head + tail) as object),
    legs: []
  })
  return { ...envelope, legs } satisfies Flows.FlowsDoc
})

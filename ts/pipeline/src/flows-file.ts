/**
 * A flows document read off a file, leg by leg — lenient, or decoded against
 * the `Flows` contract.
 *
 * The envelope — the document with its `legs` emptied — is taken as one small
 * value and each leg apart, then the two are put back together, so a capture of
 * thousands of calls is never held as one string: V8 refuses one above 512 MiB.
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

/** One part of the document as its own text: an envelope half, or one leg. */
type Part =
  | { readonly kind: "head" | "tail"; readonly text: string }
  | { readonly kind: "leg"; readonly index: number; readonly text: string }

/**
 * The document's parts, in order. A leg's text is its own JSON, the separator
 * behind it left out, so it parses alone; the head ends on the `[` of `legs` and
 * the tail opens on its `]`, which is why head + tail parse as one value with
 * the legs taken out.
 */
function* parts(file: string): Generator<Part> {
  for (const segment of segments(file)) {
    if (segment.kind === "leg") {
      yield {
        kind: "leg",
        index: segment.index,
        text: Buffer.from(segment.bytes.subarray(0, segment.json)).toString("utf8")
      }
    } else yield { kind: segment.kind, text: Buffer.from(segment.bytes).toString("utf8") }
  }
}

/**
 * The flows document at `file`, as lenient as the contract it builds: nothing
 * re-emits a flows document, so an emitter that added a field must not break
 * this reader.
 */
export const readFlowsFile = (file: string): Flows.FlowsDoc => {
  const legs: Array<Flows.Leg> = []
  let head = ""
  let tail = ""
  for (const part of parts(file)) {
    if (part.kind === "leg") legs.push(JSON.parse(part.text) as Flows.Leg)
    else if (part.kind === "head") head = part.text
    else tail = part.text
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
  for (const part of parts(file)) {
    if (part.kind === "leg") {
      legs.push(
        yield* decodeLeg(JSON.parse(part.text) as unknown).pipe(
          Effect.mapError((cause) =>
            new FlowsLegRefused({ file, index: part.index, reason: cause.message })
          )
        )
      )
    } else if (part.kind === "head") head = part.text
    else tail = part.text
  }
  const envelope = yield* Flows.decodeFlows({ ...(JSON.parse(head + tail) as object), legs: [] })
  return { ...envelope, legs } satisfies Flows.FlowsDoc
})

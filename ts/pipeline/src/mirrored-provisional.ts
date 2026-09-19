/**
 * A relayed provisional crosses the vantage TWICE — arriving on the leg it was
 * sent to, leaving on the leg it is relayed to — and the document has to say the
 * same thing about both halves (§6.9, issue 116).
 *
 * An unreliable provisional rides no timer, so it does not retransmit: a
 * platform that rings twice has SENT TWICE, and a B2BUA relays each one as it
 * arrives. Where the captured platform dropped one on its own account, the
 * capture holds fewer arrivals than emissions — and the replaying SUT, relaying
 * as it should, puts a datagram on a leg whose only armed step is the next
 * message. This pass gives that datagram its step.
 *
 * **One arrival per emission.** Which emission an arrival relays is
 * `relayOriginOf`'s reading, the same one the delay classification takes — but
 * that reading is many-to-one: it takes the LATEST emission inside its window,
 * so two emissions milliseconds apart collect BOTH their arrivals on the second.
 * A relay emits one datagram per datagram, so a second arrival naming a claimed
 * emission belongs to the nearest earlier unclaimed one of that status. What is
 * left unclaimed is what the capture is missing.
 *
 * **Only where the SUT's output is known.** A derived arrival copies one the
 * capture holds, so the emission it answers must be the SAME MESSAGE as one
 * already relayed — nothing here predicts what a platform makes of a message it
 * has not been seen handling. A bare provisional followed by one authorising
 * early media is two different relays, and this pass declines both. The STATUS
 * decides nothing on its own: 180 and 183 are the same class and take the same
 * path.
 *
 * **Never past the transaction's own final.** A client transaction leaves
 * Proceeding when a final arrives (RFC 3261 §17.1.1.2), so a provisional the
 * peer emits after the relaying leg has taken its own final reaches no
 * transaction user and is relayed by nobody.
 *
 * A derived arrival keeps the `observed` coordinate of the arrival it copies.
 * The SUT emits that captured message again, so that message is what both
 * datagrams are rightly compared against — `capturedMessage` is a lookup, not a
 * consumption, and the second relay keeps its header comparison instead of
 * going unreferenced. `relayed-provisional-expect-derived` names each one.
 */
import { Wire } from "@sip/contracts"
import type { ResourceFile } from "./bodies.js"
import { relayOriginOf, type StepTiming } from "./delay.js"
import type { StepDraft } from "./draft.js"
import { stepId, type StepSource } from "./flowsteps.js"

/** One expectation this pass derived, for the flag. */
export interface Mirrored {
  readonly step: string
  readonly leg: string
  readonly status: number
  /** The `send` step whose relay it is. */
  readonly relays: string
  /** The captured arrival it copies, coordinate included. */
  readonly copies: string
}

/** The build-up arrays this pass splices into, all parallel to `steps`. */
export interface MirrorInput {
  readonly steps: Array<StepDraft>
  readonly timings: Array<StepTiming>
  readonly sources: Array<StepSource>
  /** The case's stored bodies, so a ref compares by CONTENT and not by name. */
  readonly resources: ReadonlyArray<ResourceFile>
}

/**
 * An unreliable provisional — the class that never repeats, so every emission is
 * a step (§6.9).
 *
 * Both spellings of the RSeq, because a `send` and an `expect` state it
 * differently: a send stores the header, an expect states its existence through
 * `headers-present`, the stack owning the value (§9.1). Reading one spelling
 * would classify one leg of a reliable stream unreliable, which is the
 * asymmetry this pass exists to remove.
 */
const unreliable = (step: StepDraft): boolean =>
  step.msg.status !== undefined && step.msg.status > 100 && step.msg.status < 200 &&
  !(step.msg.headers ?? []).some((h) => h.name.toLowerCase() === "rseq") &&
  !(step.msg["headers-present"] ?? []).some((n) => n.toLowerCase() === "rseq")

/**
 * A message spec as a comparable value: two emissions the SUT relays alike.
 *
 * Keys are sorted at EVERY level, and a stored body is compared by its CONTENT.
 * Two spellings of one message would each defeat this: a `JSON.stringify`
 * replacer array filters property names all the way down, erasing every header
 * value; and a ref names a file per emission, so two byte-identical bodies sit
 * under different names and read as different messages.
 */
const canonical = (value: unknown, textOf: (ref: string) => string | undefined): unknown => {
  if (Array.isArray(value)) return value.map((v) => canonical(v, textOf))
  if (typeof value === "string") return textOf(value) ?? value
  if (value === null || typeof value !== "object") return value
  const out: Record<string, unknown> = {}
  for (const key of Object.keys(value as Record<string, unknown>).sort()) {
    out[key] = canonical((value as Record<string, unknown>)[key], textOf)
  }
  return out
}

const shaper = (resources: ReadonlyArray<ResourceFile>) => {
  const byPath = new Map(resources.map((r) => [r.relPath, Wire.base64Of(r.bytes)]))
  const textOf = (ref: string): string | undefined => byPath.get(ref)
  return (step: StepDraft): string => JSON.stringify(canonical(step.msg, textOf))
}

/**
 * Whether `leg` has already taken a final answering `method`'s transaction by
 * document position `before`. Past it the SUT's server transaction on that leg
 * has left Proceeding (RFC 3261 §17.2.1), so it emits no further provisional
 * there and there is no relay to derive.
 *
 * The method alone keys it, not the transaction: a provisional to a re-INVITE
 * is never derived once the initial INVITE's final stands. Conservative in the
 * direction that derives nothing.
 */
const finalTaken = (
  steps: ReadonlyArray<StepDraft>,
  leg: string,
  method: string | undefined,
  before: number
): boolean =>
  steps.slice(0, before).some((s) =>
    s.leg === leg && s.op === "expect" && (s.msg.status ?? 0) >= 200 &&
    s.msg["cseq-method"] === method
  )

/** The emission of `status` on `from`'s leg nearest before it, or -1. */
const lastEmissionBefore = (
  steps: ReadonlyArray<StepDraft>,
  from: number,
  status: number,
  isEmission: (i: number) => boolean
): number => {
  for (let p = from - 1; p >= 0; p--) {
    if (isEmission(p) && steps[p]!.leg === steps[from]!.leg && steps[p]!.msg.status === status) {
      return p
    }
  }
  return -1
}

/**
 * Give every unreliable-provisional emission the relayed arrival it causes, and
 * report what was derived. Mutates `steps`, `timings` and `sources` together —
 * three arrays that index each other by position.
 *
 * Runs BEFORE the delay classification, so a derived arrival is classified like
 * any other: `propagated`, ~0, anchored on the emission it relays.
 */
export const mirrorRelayedProvisionals = (input: MirrorInput): Array<Mirrored> => {
  const { resources, sources, steps, timings } = input
  const shape = shaper(resources)

  /** Emission index -> the arrival that relays it. Absent where nothing does. */
  const claimed = new Map<number, number>()
  const isEmission = (i: number): boolean => steps[i]!.op === "send" && unreliable(steps[i]!)
  steps.forEach((step, i) => {
    if (step.op !== "expect" || !unreliable(step)) return
    let origin = relayOriginOf(timings, i)
    // Walk back to the nearest earlier emission of this status the SUT has not
    // already been credited with: one datagram out per datagram in.
    while (origin >= 0 && claimed.has(origin)) {
      origin = lastEmissionBefore(steps, origin, step.msg.status!, isEmission)
    }
    if (origin >= 0) claimed.set(origin, i)
  })

  /** One derivation, resolved to OBJECTS before any splice moves the indices. */
  interface Derived {
    readonly at: number
    readonly copyStep: StepDraft
    readonly copySource: StepSource
    readonly copyTiming: StepTiming
    /** The relay latency the capture measured on the pair it copies (µs). */
    readonly latency_us: number
  }
  const derived: Array<Derived> = []
  steps.forEach((step, at) => {
    if (!isEmission(at) || claimed.has(at)) return
    const twin = [...claimed.keys()].find((e) => shape(steps[e]!) === shape(step))
    if (twin === undefined) return
    const copy = claimed.get(twin)!
    if (finalTaken(steps, steps[copy]!.leg, step.msg["cseq-method"], at)) return
    derived.push({
      at,
      copyStep: steps[copy]!,
      copySource: sources[copy]!,
      copyTiming: timings[copy]!,
      latency_us: Math.max(0, timings[copy]!.ts_us - timings[twin]!.ts_us)
    })
  })
  if (derived.length === 0) return []

  // Back to front: an insertion shifts every index after it, and every entry's
  // own `at` is below the ones already spliced. Ids are positional, so the flow
  // is renumbered once every insertion is in, and these records hold step
  // OBJECTS until then.
  const pending: Array<{
    readonly step: StepDraft
    readonly relays: StepDraft
    readonly copies: StepDraft
  }> = []
  for (const d of [...derived].reverse()) {
    const send = steps[d.at]!
    // The `observed` coordinate rides along: the SUT emits that captured
    // message again, so that message is what this datagram is compared against.
    const step: StepDraft = { ...d.copyStep, msg: { ...d.copyStep.msg } }
    steps.splice(d.at + 1, 0, step)
    timings.splice(d.at + 1, 0, {
      ...d.copyTiming,
      // The captured relay latency, so the arrival lands where a relay of THIS
      // emission would.
      ts_us: timings[d.at]!.ts_us + d.latency_us
    })
    sources.splice(d.at + 1, 0, { ...d.copySource })
    pending.push({ step, relays: send, copies: d.copyStep })
  }

  steps.forEach((step, i) => {
    step.id = stepId(i + 1)
  })
  sources.forEach((source, i) => {
    sources[i] = { ...source, id: stepId(i + 1) }
  })
  return pending.reverse().map((p) => ({
    step: p.step.id,
    leg: p.step.leg,
    status: p.step.msg.status!,
    relays: p.relays.id,
    copies: p.copies.id
  }))
}

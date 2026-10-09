/**
 * The post-run confrontation: re-reads a finished run bundle beside its source
 * document and states every difference between what the capture shows and what
 * the run recorded, as pure {@link Probe} values.
 *
 * Four parts, matching what a run leaves behind:
 *
 * - **headers** — each recorded reception the interpreter attributed to an
 *   `expect` step is paired, through the step's `observed` coordinate, with the
 *   captured message in the capture's legs, and both verbatim datagrams are
 *   diffed per header name under that name's fold. A reception with no
 *   coordinate (or no captured leg supplied) compared NOTHING and is counted
 *   `unreferenced` — never an empty difference list. Each probe also carries
 *   the relay input the run drove for that message, so a rule can tell a header
 *   the system dropped from one it was never handed.
 * - **bodies** — each such reception whose `expect` states a text resource is
 *   held against that resource's text, both sides folded under the
 *   expectation's `compare` mode; the expected texts come from the caller,
 *   keyed by ref, and a ref the caller did not supply is the caller's error.
 *   Under `sdp` the fold masks the lane-owned fields the expect's `rewrite`
 *   names only where the run's media mode says the lane rebooked them, and
 *   on a verbatim run holds the two texts to the same bytes, an origin the
 *   replayed endpoint mints marked on its `o=` row (`./sdporigin.ts`).
 * - **shape** — the verdict's structural failures, restated in the delta-record
 *   vocabulary (a final answered with another status, a datagram nothing
 *   expected — serviced by the leg or not — an expectation nothing satisfied).
 * - **retransmission** — a declared ACK-hold's evidenced re-pass count against
 *   the re-passed un-ACKed 2xx finals the recording actually shows.
 *
 * Classification is not here: probes are handed to the {@link Classifier}
 * seam, and the driver writes the classified rows.
 */
import type { Bundle, Deviation, Flow } from "@sip/contracts"
import { Body, Confrontation, Flows, Pivot, Wire } from "@sip/contracts"
import { carriesBody, mimeKey } from "./bodies.js"
import { bodiesEqual } from "./bodyfold.js"
import { layoutFault, locate, type LayoutFault } from "./parts.js"
import { diffSdp, maskOf } from "./sdpfold.js"
import { answering, collecting, type Driven, identitiesIn, type Ledger, readOrigins, type Reception, type Sighting } from "./sdporigin.js"
import type { CaseContext, Classification, DocumentStep, UnackedFinal } from "./classifier.js"
import { items, valuesEqual } from "./fold.js"
import { legPlaces } from "./leg-role.js"
import { receptionsOf } from "./receptions.js"
import type { BodyProbe, HeaderProbe, MsgScope, Probe } from "./probe.js"
import { probeSetDelta, scopeText, shapeSides, signature } from "./probe.js"
import type { WireHeader } from "./wire.js"
import {
  bodyBytesOf,
  canonicalName,
  headersInOrder,
  headersInOrderRaw,
  headerValuesOf,
  headText,
  startLineOf
} from "./wire.js"

/** Leg index in the flows document → the leg. */
export type CapturedLegs = ReadonlyMap<number, Flows.Leg>

/** Every leg of a whole document, for a caller that holds one. */
export const capturedOf = (flows: Flows.FlowsDoc): CapturedLegs =>
  new Map(flows.legs.map((leg, index) => [index, leg]))

export interface ConfrontInput {
  readonly pivot: Pivot.PivotV3
  readonly verdict: Bundle.RunVerdict
  /** Leg name → the leg's recording, in wire order. */
  readonly recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
  /**
   * The captured legs the case's `observed` coordinates name, by the index the
   * flows document gives them — the few this case cites, never the capture.
   * Absent for an authored case.
   */
  readonly captured?: CapturedLegs
  /** Resource ref → its bytes, for every resource body or part a `send` or `expect` step states. */
  readonly resources?: ReadonlyMap<string, Uint8Array>
  /** What the run's media plane did to its session descriptions; absent reads as `rebooked`. */
  readonly media?: Bundle.MediaMode
}

/** One probe, at the flow step it was observed at (empty when unattributed). */
export interface ProbeAt {
  readonly step: string
  readonly probe: Probe
}

export interface Confronted {
  readonly probes: ReadonlyArray<ProbeAt>
  /** Receptions whose captured reference was found and compared. */
  readonly compared: number
  /** Receptions that compared nothing — no coordinate, or no captured leg. */
  readonly unreferenced: number
  readonly context: CaseContext
}

export const confront = (input: ConfrontInput): Confronted => {
  const steps = new Map(Pivot.pivotSteps(input.pivot).map((step) => [step.id, step]))
  const inbound = inboundEvidence(input.pivot)
  const shape = shapeProbes(input.verdict, steps, input.recordings)
  const headers = headerProbes(input, steps, inbound)
  const placeOf = legPlaces(input.pivot.calls)
  const placed = (at: ProbeAt): ProbeAt => {
    const leg = steps.get(at.step)?.leg
    return leg === undefined ? at : { ...at, probe: { ...at.probe, leg: placeOf(leg) } }
  }
  const probes = [
    ...shape,
    ...retransmissionProbes(input.pivot.deviations ?? [], input.recordings),
    ...suppressCrossFinal(shape, [...headers.probes, ...bodyProbes(input, steps)])
  ].map(placed)
  return {
    probes,
    compared: headers.compared,
    unreferenced: headers.unreferenced,
    context: {
      relay18x: input.pivot.calls.flatMap((call) => (call.relay18x ? [call.relay18x] : [])),
      run: runShape(input.verdict, input.recordings),
      document: documentShape(input.pivot)
    }
  }
}

/** A `send` of a final response to an INVITE — the message that draws an ACK back. */
const isInviteFinal = (step: Flow.Step): boolean =>
  step.op === "send" &&
  step.msg.status !== undefined &&
  step.msg.status >= 200 &&
  (step.msg["cseq-method"] ?? "").toUpperCase() === "INVITE"

const isAckExpect = (step: Flow.Step): boolean =>
  step.op === "expect" && (step.msg.method ?? "").toUpperCase() === "ACK"

/**
 * The document's own shape, read off the flow alone — nothing about the run
 * enters here. The ACK search runs per leg and stops at that leg's NEXT INVITE
 * final, so a re-INVITE's ACK can never stand in for the one the transaction
 * before it is owed.
 */
const documentShape = (pivot: Pivot.PivotV3): CaseContext["document"] => {
  const steps = Pivot.pivotSteps(pivot)
  const unackedFinals: Array<UnackedFinal> = []
  for (const [i, step] of steps.entries()) {
    const status = step.msg.status
    if (status === undefined || !isInviteFinal(step)) continue
    const after = steps.slice(i + 1).filter((s) => s.leg === step.leg)
    const next = after.findIndex(isInviteFinal)
    const window = next === -1 ? after : after.slice(0, next)
    if (window.some(isAckExpect)) continue
    unackedFinals.push({
      step: step.id,
      leg: step.leg,
      status,
      legTail: after.length === 0,
      repeated: (step.retransmits ?? 0) > 0
    })
  }
  const attempts = pivot.calls.flatMap((call) =>
    call.attempts.map((a) => ({
      call: call.id,
      callerLeg: call.caller_leg,
      leg: a.leg,
      ...(a.final === undefined ? {} : { status: a.final.status }),
      ...(a.cause === undefined ? {} : { cause: a.cause })
    }))
  )
  return { unackedFinals, calls: pivot.calls.length, steps: steps.map(documentStep), attempts }
}

/** One flow step, transcribed. Absent stays absent: the document's silence is a fact. */
const documentStep = (step: Flow.Step): DocumentStep => {
  const method = step.msg.method?.toUpperCase()
  const cseqMethod = step.msg["cseq-method"]?.toUpperCase()
  return {
    id: step.id,
    leg: step.leg,
    op: step.op,
    ...(method === undefined ? {} : { method }),
    ...(step.msg.status === undefined ? {} : { status: step.msg.status }),
    ...(cseqMethod === undefined ? {} : { cseqMethod }),
    ...(step.msg.cseq === undefined ? {} : { cseq: step.msg.cseq }),
    auto: step.auto ?? false,
    optional: step.optional ?? false,
    inDialog: step.in_dialog ?? false,
    ...(carriesSdp(step.msg.body) ? { sdp: true as const } : {}),
    ...(statesReliable(step.msg) ? { reliable: true as const } : {})
  }
}

const isSdpType = (contentType: string | undefined): boolean =>
  contentType === undefined || /application\/sdp/i.test(contentType)

/** Whether a step's body is a session description, bare, framed in a multipart, or by shape. */
const carriesSdp = (body: Flow.Step["msg"]["body"]): boolean => {
  if (body === undefined) return false
  if ("multipart" in body) return body.multipart.parts.some((p) => isSdpType(p["content-type"]))
  if ("ref" in body) return isSdpType(body["content-type"])
  return body.mode === "sdp-present" || body.mode === "multipart-present"
}

/** Whether a step states the provisional reliable: an `RSeq`, or `Require: 100rel`. */
const statesReliable = (msg: Flow.Step["msg"]): boolean =>
  (msg["headers-present"] ?? []).some((n) => n.toLowerCase() === "rseq") ||
  (msg.headers ?? []).some(
    (h) =>
      h.name.toLowerCase() === "rseq" ||
      (h.name.toLowerCase() === "require" && /\b100rel\b/i.test(h.value))
  )

/** Whether a recorded datagram is a BYE request. */
const isBye = (message: Bundle.RecordedMessage): boolean => {
  const line = startLineOf(headText(message))
  return line !== undefined && line.kind === "request" && line.method === "BYE"
}

/**
 * The run's own shape, counted off the verdict and the recording. `in` is the
 * direction the scripted actor RECEIVED on, so a BYE there is one the SYSTEM
 * sent — the only teardown a leg's own recording can attribute.
 */
const runShape = (
  verdict: Bundle.RunVerdict,
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): CaseContext["run"] => {
  const legs = [...recordings.values()]
  return {
    ...(verdict.abandoned?.step === undefined ? {} : { abandonedAt: verdict.abandoned.step }),
    messages: legs.reduce((total, leg) => total + leg.length, 0),
    legs: legs.length,
    legsTornDownBySystem: legs.filter((leg) => leg.some((m) => m.dir === "in" && isBye(m))).length,
    sentRSeqs: new Map(
      [...recordings]
        .map(([leg, recording]) =>
          [leg, recording.flatMap((m) => (m.dir === "out" && m.repeat_of === undefined ? sentRSeqOf(m) : []))] as const
        )
        .filter(([, numbers]) => numbers.length > 0)
    ),
    openers: new Map(
      [...recordings].flatMap(([leg, recording]) => {
        const opener = recording.find((m) => m.repeat_of === undefined && scopeOf(m)?.kind === "initial-invite")
        return opener === undefined ? [] : [[leg, headersInOrder(opener)] as const]
      })
    ),
    receptions: receptionsOf(recordings)
  }
}

/** The `RSeq` a recorded provisional states, read as a number; none on any other datagram. */
const sentRSeqOf = (message: Bundle.RecordedMessage): ReadonlyArray<number> => {
  const head = headText(message)
  const start = startLineOf(head)
  if (start === undefined || start.kind !== "response" || start.status <= 100 || start.status >= 200) return []
  const value = headerValuesOf(headersInOrderRaw(head), "RSeq")[0]?.trim() ?? ""
  return /^[0-9]+$/.test(value) ? [Number(value)] : []
}

/** One classified row, ready for `confrontation.ndjson`. */
export const recordOf = (
  meta: {
    readonly lane: string
    readonly capture: string
    readonly case: string
    readonly run: number
  },
  at: ProbeAt,
  classification: Classification
): Confrontation.ConfrontationRecord => {
  const delta = probeSetDelta(at.probe)
  const [captured, replayed] =
    at.probe.kind === "shape" ? shapeSides(at.probe.shapeKind) : [at.probe.captured, at.probe.replayed]
  return {
    lane: meta.lane,
    capture: meta.capture,
    case: meta.case,
    run: meta.run,
    step: at.step,
    kind: at.probe.kind,
    signature: signature(at.probe),
    name: at.probe.kind === "header" ? at.probe.name : at.probe.kind === "body" ? at.probe.mediaType : "",
    scope: at.probe.scope === undefined ? "" : scopeText(at.probe.scope),
    captured,
    replayed,
    inbound: at.probe.kind === "header" && at.probe.inbound,
    added: delta.added,
    removed: delta.removed,
    class: classification.class,
    rule: classification.rule,
    ticket: classification.ticket
  }
}

/**
 * Every header a capture-side `send` step carries, keyed canonically: the
 * evidence that a header REACHED the replayed system. Case-wide and
 * direction-blind, exactly as broad as the capture's own sends.
 */
const inboundEvidence = (pivot: Pivot.PivotV3): ReadonlyMap<string, ReadonlyArray<string>> => {
  const out = new Map<string, Array<string>>()
  for (const step of Pivot.pivotSteps(pivot)) {
    if (step.op !== "send") continue
    for (const header of step.msg.headers ?? []) {
      const key = canonicalName(header.name)
      const list = out.get(key) ?? []
      list.push(header.value)
      out.set(key, list)
    }
  }
  return out
}

const headerProbes = (
  input: ConfrontInput,
  steps: ReadonlyMap<string, Flow.Step>,
  inbound: ReadonlyMap<string, ReadonlyArray<string>>
): { readonly probes: ReadonlyArray<ProbeAt>; readonly compared: number; readonly unreferenced: number } => {
  const probes: Array<ProbeAt> = []
  const driving = drivenIndex(input.recordings)
  let compared = 0
  let unreferenced = 0
  for (const [leg, recording] of input.recordings) {
    for (const message of recording) {
      if (message.dir !== "in" || message.repeat_of !== undefined) continue
      const step = message.step === undefined ? undefined : steps.get(message.step)
      if (step === undefined || step.op !== "expect") continue
      const reference = capturedMessage(input.captured, step.observed)
      if (reference === undefined) {
        unreferenced += 1
        continue
      }
      compared += 1
      const scope = scopeOf(message)
      if (scope === undefined) continue
      const driven = drivenNames(driving, leg, message.at_us, scope)
      const bodiless = !carriesBody(reference) && recordedBodiless(message)
      const around: ReceptionFacts = {
        replayedParts: partHeaders(message.body),
        answeredRequest: answeredRequestHeaders(recording, message, scope),
        replayedCarriesBody: !recordedBodiless(message)
      }
      for (
        const probe of diffHeaders(
          headersInOrder(reference),
          headersInOrder(message),
          scope,
          inbound,
          driven,
          step.id,
          bodiless,
          around
        )
      ) {
        probes.push({ step: step.id, probe })
      }
    }
  }
  return { probes, compared, unreferenced }
}

/**
 * One probe per recorded reception whose `expect` asserts a body the reception
 * does not carry: a resource body under the expectation's `compare` mode, a
 * multipart body part by part. A frozen body compares byte for byte, whatever
 * its bytes hold; `sdp` and `xml` fold both sides after a strict UTF-8 decode,
 * and a side that is not UTF-8 under a text compare is a probe. Under `sdp`
 * there is one probe per differing line key, each side one element per line
 * the way a header probe carries one per value. A reception with no body at
 * all is confronted too, as the empty side: the assertion stands whether or
 * not anything arrived.
 *
 * THE body is the recorded layout's: the arm keeps the whole datagram, bytes
 * past the declared `Content-Length` included (RFC 3261 §18.3 discards them),
 * and the layout's `len` bounds what every comparison reads, single and
 * multipart, under every compare mode. The interpreter writes a layout for
 * every datagram that parses and carries a body, so a line with no layout
 * reads as BODILESS (the tail it may hold is what §18.3 discards, and the gate
 * read it so), and a layout the bytes cannot honour — `len` past the tail, a
 * part past `len` — is a writer's fault and is stated once as a probe, never
 * thrown.
 *
 * A multipart reception's parts are located by that layout (never split here)
 * and matched to the expectation's parts by POSITION: one probe for a
 * part-count mismatch (the sides name each part's media type), one per part
 * whose media type differs (the sides name the two types), one per part whose
 * bytes differ under its `compare`. A part's entity headers are not compared —
 * the confrontation reads payloads, and a header delta on a part is out of
 * its scope. A multipart expect on a line that recorded no layout is one
 * probe saying so.
 *
 * A probe's sides stay strings: the text where BOTH sides are text (UTF-8
 * whose only controls are tab, CR and LF), standard base64 on both where
 * either is not.
 *
 * On a verbatim run every `sdp` pair of the cell is read by one origin
 * reading (`./sdporigin.ts`), over the origins the capture's `send` steps and
 * the run's own sends drove into the endpoint, each pair placed in the
 * capture's flow order and in the replay's wire order across the legs, so a
 * session carried from one leg onto another is held to the same pairing. The
 * walk runs twice: once to note the pairs, once with their answers. The
 * probes are returned leg by leg.
 */
export const bodyProbes = (
  input: ConfrontInput,
  steps: ReadonlyMap<string, Flow.Step>
): ReadonlyArray<ProbeAt> => {
  const media = input.media ?? "rebooked"
  const legs = [...input.recordings.values()]
  const wire = legs
    .flatMap((recording, leg) => recording.map((message, at) => ({ leg, at, message })))
    .sort((a, b) => a.message.at_us - b.message.at_us || a.leg - b.leg || a.at - b.at)
  const flowAt = new Map([...steps.keys()].map((id, at) => [id, at]))
  const walk = (origins: Ledger | undefined): ReadonlyMap<Bundle.RecordedMessage, ReadonlyArray<ProbeAt>> => {
    const found = new Map<Bundle.RecordedMessage, ReadonlyArray<ProbeAt>>()
    wire.forEach(({ message }, replayedAt) => {
      const capturedAt = message.step === undefined ? Number.MAX_SAFE_INTEGER : flowAt.get(message.step) ?? Number.MAX_SAFE_INTEGER
      found.set(message, messageBodyProbes(input, steps, message, media, origins?.at({ capturedAt, replayedAt })))
    })
    return found
  }
  let found: ReadonlyMap<Bundle.RecordedMessage, ReadonlyArray<ProbeAt>>
  if (media === "verbatim") {
    const sightings: Array<Sighting> = []
    walk(collecting(sightings))
    found = walk(answering(readOrigins(sightings, drivenOrigins(input))))
  } else {
    found = walk(undefined)
  }
  return legs.flatMap((recording) => recording.flatMap((message) => found.get(message) ?? []))
}

/** One reception's body probes; none where it answers no `expect` stating a body. */
const messageBodyProbes = (
  input: ConfrontInput,
  steps: ReadonlyMap<string, Flow.Step>,
  message: Bundle.RecordedMessage,
  media: Bundle.MediaMode,
  origins: Reception | undefined
): ReadonlyArray<ProbeAt> => {
  if (message.dir !== "in" || message.repeat_of !== undefined) return []
  const step = message.step === undefined ? undefined : steps.get(message.step)
  if (step === undefined || step.op !== "expect") return []
  const body = step.msg.body
  if (body === undefined) return []
  const scope = scopeOf(message)
  if (scope === undefined) return []
  if (Body.isResourceBody(body)) {
    const mediaType = mediaTypeOf(body["content-type"], message)
    const received = recordedBody(message)
    const probes = "fault" in received
      ? [faultProbe(step.id, mediaType, scope, received.fault)]
      : bodyProbe(step.id, body, mediaType, scope, expectedBytes(input.resources, body.ref), received.bytes, media, origins)
    return probes.map((probe) => ({ step: step.id, probe }))
  }
  if (Body.isMultipartBody(body)) {
    return multipartProbes(step.id, body.multipart, scope, input.resources, message, media, origins)
      .map((probe) => ({ step: step.id, probe }))
  }
  return []
}

/**
 * The origin identities each side drove into the endpoint: the texts of the
 * capture's `send` bodies the caller supplied, and every body the run sent.
 */
const drivenOrigins = (input: ConfrontInput): Driven => {
  const captured: Array<string> = []
  for (const step of Pivot.pivotSteps(input.pivot)) {
    const body = step.msg.body
    if (step.op !== "send" || body === undefined) continue
    const refs = Body.isResourceBody(body)
      ? [body.ref]
      : Body.isMultipartBody(body)
      ? body.multipart.parts.map((part) => part.ref)
      : []
    for (const ref of refs) {
      const text = textOfResource(input.resources?.get(ref))
      if (text !== undefined) captured.push(text)
    }
  }
  const replayed: Array<string> = []
  for (const recording of input.recordings.values()) {
    for (const message of recording) {
      if (message.dir !== "out") continue
      const received = recordedBody(message)
      const text = "fault" in received ? undefined : Wire.utf8Of(received.bytes)
      if (text !== undefined) replayed.push(text)
    }
  }
  return { captured: identitiesIn(captured), replayed: identitiesIn(replayed) }
}

const textOfResource = (bytes: Uint8Array | undefined): string | undefined =>
  bytes === undefined ? undefined : Wire.utf8Of(bytes)

/** The bytes a resource ref names; a ref the caller did not supply is the caller's error. */
const expectedBytes = (resources: ReadonlyMap<string, Uint8Array> | undefined, ref: string): Uint8Array => {
  const bytes = resources?.get(ref)
  if (bytes === undefined) throw new Error(`the document expects body resource ${ref}, which the driver did not supply`)
  return bytes
}

/**
 * The bare `type/subtype` a body record is keyed by: the expectation's
 * `content-type`, or the reception's own `Content-Type` where the expectation
 * states none.
 */
const mediaTypeOf = (declared: string | undefined, message: Wire.Msg): string =>
  mimeKey(declared ?? headerValuesOf(headersInOrder(message), "Content-Type")[0] ?? "")

/**
 * The received body under the one bound: the layout's `len`. A line that
 * states no layout carries no body — the interpreter writes a layout for every
 * parsable datagram that carries one, and the gate read that line as bodiless
 * — whatever tail it holds (the bytes RFC 3261 §18.3 discards).
 */
const recordedBody = (message: Bundle.RecordedMessage): { readonly bytes: Uint8Array } | { readonly fault: LayoutFault } => {
  if (message.body === undefined) return { bytes: new Uint8Array(0) }
  const tail = bodyBytesOf(message)
  const fault = layoutFault(tail, message.body)
  return fault === undefined ? { bytes: tail.subarray(0, message.body.len) } : { fault }
}

/** Whether a recorded line carries a body under the one bound. */
const recordedBodiless = (message: Bundle.RecordedMessage): boolean => message.body === undefined || message.body.len === 0

const faultProbe = (step: string, mediaType: string, scope: MsgScope, fault: LayoutFault): BodyProbe => ({
  kind: "body",
  step,
  mediaType,
  scope,
  compare: "exact",
  captured: [fault.stated],
  replayed: [fault.carried]
})

/** Text is UTF-8 whose only control characters are tab, CR and LF. */
const textOf = (bytes: Uint8Array): string | undefined => {
  for (const byte of bytes) {
    if (byte === 0x7f || (byte < 0x20 && byte !== 0x09 && byte !== 0x0a && byte !== 0x0d)) return undefined
  }
  return Wire.utf8Of(bytes)
}

/** Two probe sides, rendered alike: the texts where both are text, standard base64 on both where either is not. */
const sidesOf = (captured: Uint8Array, replayed: Uint8Array): readonly [string, string] => {
  const a = textOf(captured)
  const b = textOf(replayed)
  return a !== undefined && b !== undefined ? [a, b] : [Wire.base64Of(captured), Wire.base64Of(replayed)]
}

/** What a resource body or a part states about its comparison. */
interface Compared {
  readonly compare?: Body.BodyCompare
  readonly rewrite?: ReadonlyArray<string>
}

const bodyProbe = (
  step: string,
  body: Compared,
  mediaType: string,
  scope: MsgScope,
  captured: Uint8Array,
  replayed: Uint8Array,
  media: Bundle.MediaMode,
  origins: Reception | undefined
): ReadonlyArray<BodyProbe> => {
  const compare = body.compare ?? "exact"
  if (compare === "exact") {
    if (bytesEqual(captured, replayed)) return []
    const [a, b] = sidesOf(captured, replayed)
    return [{ kind: "body", step, mediaType, scope, compare, captured: [a], replayed: [b] }]
  }
  // A text compare reads both sides as UTF-8; a side that is none differs
  // from anything, and both are shown as the bytes they are.
  const capturedText = Wire.utf8Of(captured)
  const replayedText = Wire.utf8Of(replayed)
  if (capturedText === undefined || replayedText === undefined) {
    const [a, b] = sidesOf(captured, replayed)
    return [{ kind: "body", step, mediaType, scope, compare, captured: [a], replayed: [b] }]
  }
  if (compare === "sdp") {
    // Every pair is read, equal ones included, so each is held to the
    // sessions the cell shows.
    const minted = origins?.read(capturedText, replayedText) ?? false
    return diffSdp(maskOf(body.rewrite, media), capturedText, replayedText, minted).map((d) => ({
      kind: "body",
      step,
      mediaType,
      scope,
      compare,
      captured: d.captured,
      replayed: d.replayed,
      sdp: { section: d.section, line: d.line, ...(d.mintedOrigin === true ? { mintedOrigin: true as const } : {}) }
    }))
  }
  if (bodiesEqual(compare, capturedText, replayedText)) return []
  return [{ kind: "body", step, mediaType, scope, compare, captured: [capturedText], replayed: [replayedText] }]
}

const bytesEqual = (a: Uint8Array, b: Uint8Array): boolean => {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false
  return true
}

/**
 * The multipart confrontation: the received parts, located by the recording's
 * layout, against the expectation's, by position. A line with no layout
 * states no parts to locate: one probe says so, because the interpreter
 * writes a layout for every parsable datagram that carries a body and its
 * absence is a fact about the reception, not a count of zero.
 */
const multipartProbes = (
  step: string,
  expected: Body.Multipart,
  scope: MsgScope,
  resources: ReadonlyMap<string, Uint8Array> | undefined,
  message: Bundle.RecordedMessage,
  media: Bundle.MediaMode,
  origins: Reception | undefined
): ReadonlyArray<BodyProbe> => {
  const mediaType = mimeKey(expected["content-type"])
  if (message.body === undefined) {
    return [faultProbe(step, mediaType, scope, {
      stated: `a ${mediaType} body of ${expected.parts.length} parts`,
      carried: "the recorded line states no body layout to locate parts by"
    })]
  }
  const fault = layoutFault(bodyBytesOf(message), message.body)
  if (fault !== undefined) return [faultProbe(step, mediaType, scope, fault)]
  const received = locate(message, message.body).parts
  if (received.length !== expected.parts.length) {
    return [{
      kind: "body",
      step,
      mediaType,
      scope,
      compare: "exact",
      captured: expected.parts.map((p) => mimeKey(p["content-type"])),
      replayed: received.map((p) => mimeKey(p.contentType))
    }]
  }
  return expected.parts.flatMap((part, n) => {
    const expectedType = mimeKey(part["content-type"])
    const receivedType = mimeKey(received[n]!.contentType)
    if (expectedType !== receivedType) {
      return [{ kind: "body" as const, step, mediaType: expectedType, scope, compare: "exact" as const, captured: [expectedType], replayed: [receivedType] }]
    }
    return bodyProbe(step, part, expectedType, scope, expectedBytes(resources, part.ref), received[n]!.bytes, media, origins)
  })
}

/** One datagram the run put on the wire toward the system, in run order. */
interface Driving {
  readonly leg: string
  readonly at_us: number
  readonly scope: MsgScope | undefined
  readonly names: ReadonlySet<string>
}

/** Every outbound datagram of the run, oldest first — the inputs the system saw. */
const drivenIndex = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyArray<Driving> => {
  const out: Array<Driving> = []
  for (const [leg, recording] of recordings) {
    for (const message of recording) {
      if (message.dir !== "out") continue
      out.push({
        leg,
        at_us: message.at_us,
        scope: scopeOf(message),
        names: new Set(headersInOrder(message).map((h) => canonicalName(h.name)))
      })
    }
  }
  return out.sort((a, b) => a.at_us - b.at_us || a.leg.localeCompare(b.leg))
}

/**
 * The header names the run drove into the system as this reception's relay
 * input, or `undefined` when it drove none.
 *
 * A relay is PROMPT — the system emits what it was just handed — so the input
 * is the last datagram the run sent on any OTHER leg before the reception, and
 * only when that datagram is the same message the reception is. Anything else
 * means the system minted this message rather than relaying one, and the run
 * states no relay input at all.
 */
const drivenNames = (
  driving: ReadonlyArray<Driving>,
  leg: string,
  at_us: number,
  scope: MsgScope
): ReadonlySet<string> | undefined => {
  let last: Driving | undefined
  for (const candidate of driving) {
    if (candidate.at_us > at_us) break
    if (candidate.leg === leg) continue
    last = candidate
  }
  if (last === undefined || last.scope === undefined) return undefined
  return sameScope(last.scope, scope) ? last.names : undefined
}

const sameScope = (a: MsgScope, b: MsgScope): boolean => {
  if (a.kind !== b.kind) return false
  if (a.kind === "initial-invite") return true
  if (a.kind === "request" && b.kind === "request") {
    return a.method === b.method && a.inDialog === b.inDialog
  }
  if (a.kind === "response" && b.kind === "response") {
    return a.status === b.status && a.cseqMethod === b.cseqMethod
  }
  return false
}

const capturedMessage = (
  captured: CapturedLegs | undefined,
  observed: Flow.Observed | undefined
): Flows.Msg | undefined => {
  if (captured === undefined || observed === undefined) return undefined
  return captured.get(observed.leg)?.msgs[observed.msg]
}

/** The scope of a recorded datagram, read off its own start line and To tag. */
export const scopeOf = (message: Wire.Msg): MsgScope | undefined => scopeOfRaw(headText(message))

/** {@link scopeOf} over a head rendered as text. */
export const scopeOfRaw = (raw: string): MsgScope | undefined => {
  const start = startLineOf(raw)
  if (start === undefined) return undefined
  const headers = headersInOrderRaw(raw)
  if (start.kind === "response") {
    const cseq = headerValuesOf(headers, "CSeq")[0] ?? ""
    const method = cseq.trim().split(/\s+/)[1] ?? ""
    return { kind: "response", status: start.status, cseqMethod: method.toUpperCase() }
  }
  const toTag = /;\s*tag\s*=/i.test(headerValuesOf(headers, "To")[0] ?? "")
  if (start.method === "INVITE" && !toTag) return { kind: "initial-invite" }
  return { kind: "request", method: start.method, inDialog: toTag }
}

/**
 * The entity headers the parts of a recorded multipart body state, keyed
 * canonically, values in body order: each part's Content-Type and Content-ID
 * beside its other headers, as the layout records them. Empty for a body that
 * is no multipart, or a line with no layout.
 */
const partHeaders = (layout: Flows.MsgBody | undefined): ReadonlyMap<string, ReadonlyArray<string>> => {
  const out = new Map<string, Array<string>>()
  const add = (name: string, value: string) => {
    const key = canonicalName(name)
    out.set(key, [...(out.get(key) ?? []), value])
  }
  for (const part of layout?.parts ?? []) {
    add("Content-Type", part.content_type)
    if (part.content_id !== undefined) add("Content-ID", part.content_id)
    for (const header of part.headers ?? []) add(header.name, header.value)
  }
  return out
}

/**
 * On a recorded response, the headers of the request it answers as the run
 * sent it on the same leg (its CSeq, number and method), keyed canonically;
 * empty on a request or where the leg sent no such request.
 */
const answeredRequestHeaders = (
  recording: ReadonlyArray<Bundle.RecordedMessage>,
  response: Bundle.RecordedMessage,
  scope: MsgScope
): ReadonlyMap<string, ReadonlyArray<string>> => {
  if (scope.kind !== "response") return new Map()
  const cseqOf = (m: Wire.Msg) => headerValuesOf(headersInOrder(m), "CSeq")[0]?.trim().split(/\s+/).join(" ").toUpperCase()
  const cseq = cseqOf(response)
  const request = recording.find(
    (m) => m.dir === "out" && m.repeat_of === undefined && startLineOf(headText(m))?.kind === "request" && cseqOf(m) === cseq
  )
  const out = new Map<string, Array<string>>()
  for (const header of request === undefined ? [] : headersInOrder(request)) {
    const key = canonicalName(header.name)
    out.set(key, [...(out.get(key) ?? []), header.value])
  }
  return out
}

/** What the recording states around one reception, beside its own headers. */
export interface ReceptionFacts {
  /** What the replayed body's multipart parts state ({@link partHeaders}). */
  readonly replayedParts?: ReadonlyMap<string, ReadonlyArray<string>>
  /** The headers of the request a response answers ({@link answeredRequestHeaders}). */
  readonly answeredRequest?: ReadonlyMap<string, ReadonlyArray<string>>
  /** Whether the replayed message carries a body; absent reads as `!bodiless`. */
  readonly replayedCarriesBody?: boolean
}

/**
 * One probe per header name whose two sides disagree under the name's fold.
 * First-appearance order, captured side first; a side that never states the
 * name is the empty occurrence list, and a name whose items are empty on both
 * sides states nothing on either. `around` carries what the recording states
 * beside the message ({@link ReceptionFacts}); it never makes a probe itself.
 */
export const diffHeaders = (
  captured: ReadonlyArray<WireHeader>,
  replayed: ReadonlyArray<WireHeader>,
  scope: MsgScope,
  inbound: ReadonlyMap<string, ReadonlyArray<string>>,
  driven?: ReadonlySet<string>,
  step = "",
  bodiless = false,
  around: ReceptionFacts = {}
): ReadonlyArray<HeaderProbe> => {
  const order: Array<string> = []
  const names = new Map<string, { name: string; captured: Array<string>; replayed: Array<string> }>()
  const side = (list: ReadonlyArray<WireHeader>, which: "captured" | "replayed") => {
    for (const header of list) {
      const key = canonicalName(header.name)
      let entry = names.get(key)
      if (entry === undefined) {
        entry = { name: header.name, captured: [], replayed: [] }
        names.set(key, entry)
        order.push(key)
      }
      entry[which].push(header.value)
    }
  }
  side(captured, "captured")
  side(replayed, "replayed")
  const out: Array<HeaderProbe> = []
  for (const key of order) {
    const entry = names.get(key)
    if (entry === undefined) continue
    if (valuesEqual(entry.name, entry.captured, entry.replayed)) continue
    // A side whose items are empty states nothing; both empty is no difference.
    if (
      items(entry.name, entry.captured).length === 0 &&
      items(entry.name, entry.replayed).length === 0
    ) {
      continue
    }
    out.push({
      kind: "header",
      step,
      name: entry.name,
      scope,
      captured: entry.captured,
      replayed: entry.replayed,
      inbound: inbound.has(key),
      inboundValues: inbound.get(key) ?? [],
      driven: driven === undefined ? undefined : driven.has(key),
      bodiless,
      inReplayedParts: around.replayedParts?.get(key) ?? [],
      inAnsweredRequest: around.answeredRequest?.get(key) ?? [],
      replayedCarriesBody: around.replayedCarriesBody ?? !bodiless
    })
  }
  return out
}

/** The verdict's structural failures, restated in the delta-record vocabulary. */
export const shapeProbes = (
  verdict: Bundle.RunVerdict,
  steps: ReadonlyMap<string, Flow.Step>,
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyArray<ProbeAt> => {
  const answered = answeredTransactions(recordings)
  const out: Array<ProbeAt> = []
  for (const failure of verdict.failures ?? []) {
    switch (failure.failure) {
      case "unmatched-datagram": {
        const { arrived, gated_on } = failure
        const step = steps.get(failure.step)
        if (arrived.kind === "response" && gated_on.kind === "response") {
          out.push({
            step: failure.step,
            probe: {
              kind: "shape",
              step: failure.step,
              shapeKind: {
                shape: "status-substitution",
                captured: gated_on.status,
                observed: arrived.status
              },
              scope: substitutionScope(step, arrived.status)
            }
          })
          break
        }
        if (arrived.kind === "request" && gated_on.kind === "request") {
          out.push({
            step: failure.step,
            probe: {
              kind: "shape",
              step: failure.step,
              shapeKind: {
                shape: "method-substitution",
                expected: gated_on.method.toUpperCase(),
                observed: arrived.method
              },
              scope: undefined
            }
          })
          break
        }
        out.push(strayProbe(failure.step, failure.leg, failure.arrived, answered))
        break
      }
      case "unexpected-datagram":
      case "datagram-after-flow":
        out.push(strayProbe("", failure.leg, failure.arrived, answered))
        break
      case "expect-timed-out":
        out.push({
          step: failure.step,
          probe: {
            kind: "shape",
            step: failure.step,
            shapeKind: {
              shape: "missing-message",
              method: failure.gated_on.trim().replace(/\s+/g, "-")
            },
            scope: undefined
          }
        })
        break
      default:
        break
    }
  }
  return out
}

/**
 * Per leg, the `<CSeq number> <METHOD>` transactions the leg ANSWERED — every
 * response it emitted, whoever composed it: a background policy, the
 * interpreter's own stack, or the generic close. The delta model asks of a
 * stray request only whether it was serviced, so the composer is deliberately
 * not part of the key; `verdict.abandoned.closed` and the recording's own
 * `note` keep that distinction for whoever reads the bundle.
 *
 * Call-ID is deliberately not in the key: a leg IS one symbolic dialog (§5), so
 * CSeq number plus method already names one transaction on it, and the only
 * traffic riding a leg from another CSeq space is out-of-dialog (a keepalive),
 * which the method separates.
 */
const answeredTransactions = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyMap<string, ReadonlySet<string>> => {
  const out = new Map<string, ReadonlySet<string>>()
  for (const [leg, recording] of recordings) {
    const answered = new Set<string>()
    for (const message of recording) {
      if (message.dir !== "out") continue
      const start = startLineOf(headText(message))
      if (start === undefined || start.kind !== "response") continue
      const cseq = (headerValuesOf(headersInOrder(message), "CSeq")[0] ?? "").trim()
      const [number = "", method = ""] = cseq.split(/\s+/)
      answered.add(`${number} ${method.toUpperCase()}`)
    }
    out.set(leg, answered)
  }
  return out
}

/**
 * A datagram the script refused, as a shape: `serviced-stray` where the leg
 * answered it, `extra-message` where nothing did. The split is a real
 * difference and not a rendering — a teardown BYE the peer answered 200 is
 * RFC 3261 §15.1.2 running, one nobody answered is a transaction left open —
 * so the two carry different signatures and a lane blesses them separately.
 * Only a REQUEST can be serviced; a stray response answers nothing and stays an
 * extra message.
 */
const strayProbe = (
  step: string,
  leg: string,
  arrived: Bundle.Arrived,
  answered: ReadonlyMap<string, ReadonlySet<string>>
): ProbeAt => {
  const serviced =
    arrived.kind === "request" &&
    (answered.get(leg)?.has(`${arrived.cseq} ${arrived.method.toUpperCase()}`) ?? false)
  return {
    step,
    probe: {
      kind: "shape",
      step,
      shapeKind: serviced
        ? { shape: "serviced-stray", method: arrivedToken(arrived), action: "auto-reacted" }
        : { shape: "extra-message", method: arrivedToken(arrived) },
      scope: undefined
    }
  }
}

/** The scope a substitution pins: the initial-INVITE final, or the answered transaction. */
const substitutionScope = (step: Flow.Step | undefined, observed: number): MsgScope => {
  const cseqMethod = step?.msg["cseq-method"] ?? ""
  const declared = step?.msg.status
  const initial =
    cseqMethod === "INVITE" && step?.in_dialog !== true && declared !== undefined && declared >= 200
  return initial
    ? { kind: "initial-invite" }
    : { kind: "response", status: observed, cseqMethod }
}

const arrivedToken = (arrived: Bundle.Arrived): string =>
  arrived.kind === "request"
    ? arrived.method
    : arrived.kind === "response"
      ? String(arrived.status)
      : "unreadable"

/**
 * A step that carries a status substitution keeps the substitution as the ONE
 * record of that message: its response-scoped header and body probes hold two
 * different messages side by side and are dropped.
 */
const suppressCrossFinal = (
  shape: ReadonlyArray<ProbeAt>,
  compared: ReadonlyArray<ProbeAt>
): ReadonlyArray<ProbeAt> => {
  const substituted = new Set(
    shape
      .filter(
        (p) =>
          p.probe.kind === "shape" &&
          p.probe.shapeKind.shape === "status-substitution" &&
          p.probe.shapeKind.captured !== p.probe.shapeKind.observed
      )
      .map((p) => p.step)
  )
  if (substituted.size === 0) return compared
  return compared.filter(
    (p) =>
      !(substituted.has(p.step) && p.probe.kind !== "shape" && p.probe.scope.kind === "response")
  )
}

/**
 * The retransmission confrontation: while a declared hold kept an ACK away,
 * did the system re-pass its un-ACKed 2xx as often as the capture evidences?
 * Both sides are summed over the case — the replay's CSeq numbering is its
 * own, so one declared hold cannot be joined to one replayed transaction.
 */
export const retransmissionProbes = (
  deviations: ReadonlyArray<Deviation.Deviation>,
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): ReadonlyArray<ProbeAt> => {
  const holds = deviations.filter((d) => d.kind === "delayed-automatic")
  if (holds.length === 0) return []
  const declared = holds.reduce((sum, d) => sum + (d.retransmits ?? 0), 0)
  const step = holds.map((d) => d.step).find((s) => s !== undefined) ?? ""
  const replayed = observedRepasses(recordings)
  if (replayed === declared) return []
  return [
    {
      step,
      probe: {
        kind: "shape",
        step,
        shapeKind: { shape: "retransmission", cseqMethod: "INVITE", captured: declared, replayed },
        scope: undefined
      }
    }
  ]
}

/**
 * Re-passed 2xx (INVITE) finals per transaction — keyed Call-ID + CSeq number
 * + To tag, so a forked second 2xx stays a distinct final — counted only while
 * the ACK that answers the transaction is still absent.
 */
const observedRepasses = (
  recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
): number => {
  let count = 0
  for (const [, recording] of recordings) {
    const finals = new Map<string, Array<number>>()
    const acks = new Map<string, number>()
    for (const message of recording) {
      const start = startLineOf(headText(message))
      if (start === undefined) continue
      const headers = headersInOrder(message)
      const callId = headerValuesOf(headers, "Call-ID")[0] ?? ""
      const cseq = (headerValuesOf(headers, "CSeq")[0] ?? "").trim()
      const [cseqNumber = "", cseqMethod = ""] = cseq.split(/\s+/)
      if (message.dir === "in" && start.kind === "response") {
        if (start.status < 200 || start.status >= 300 || cseqMethod.toUpperCase() !== "INVITE") continue
        const toTag = /;\s*tag\s*=\s*([^;]+)/i.exec(headerValuesOf(headers, "To")[0] ?? "")?.[1] ?? ""
        const key = `${callId}\0${cseqNumber}\0${toTag.trim()}`
        const list = finals.get(key) ?? []
        list.push(message.at_us)
        finals.set(key, list)
      }
      if (message.dir === "out" && start.kind === "request" && start.method === "ACK") {
        const key = `${callId}\0${cseqNumber}`
        if (!acks.has(key)) acks.set(key, message.at_us)
      }
    }
    for (const [key, times] of finals) {
      const [callId = "", cseqNumber = ""] = key.split("\0")
      const ackAt = acks.get(`${callId}\0${cseqNumber}`)
      count += times.slice(1).filter((at) => ackAt === undefined || at < ackAt).length
    }
  }
  return count
}

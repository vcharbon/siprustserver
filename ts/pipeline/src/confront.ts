/**
 * The post-run confrontation: re-reads a finished run bundle beside its source
 * document and states every difference between what the capture shows and what
 * the run recorded, as pure {@link Probe} values.
 *
 * Three halves, matching what a run leaves behind:
 *
 * - **headers** — each recorded reception the interpreter attributed to an
 *   `expect` step is paired, through the step's `observed` coordinate, with the
 *   captured message in the flows document, and both verbatim datagrams are
 *   diffed per header name under that name's fold. A reception with no
 *   coordinate (or no flows document supplied) compared NOTHING and is counted
 *   `unreferenced` — never an empty difference list. Each probe also carries
 *   the relay input the run drove for that message, so a rule can tell a header
 *   the system dropped from one it was never handed.
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
import { Confrontation, Flows, Pivot } from "@sip/contracts"
import type { CaseContext, Classification, DocumentStep, UnackedFinal } from "./classifier.js"
import { items, valuesEqual } from "./fold.js"
import type { HeaderProbe, MsgScope, Probe } from "./probe.js"
import { probeSetDelta, scopeText, shapeSides, signature } from "./probe.js"
import type { WireHeader } from "./wire.js"
import { canonicalName, headersInOrder, headersInOrderRaw, headerValuesOf, startLineOf } from "./wire.js"

export interface ConfrontInput {
  readonly pivot: Pivot.PivotV3
  readonly verdict: Bundle.RunVerdict
  /** Leg name → the leg's recording, in wire order. */
  readonly recordings: ReadonlyMap<string, ReadonlyArray<Bundle.RecordedMessage>>
  /** The capture-side flows document; absent for an authored case. */
  readonly flows?: Flows.FlowsDoc
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
  /** Receptions that compared nothing — no coordinate, or no flows document. */
  readonly unreferenced: number
  readonly context: CaseContext
}

export const confront = (input: ConfrontInput): Confronted => {
  const steps = new Map(Pivot.pivotSteps(input.pivot).map((step) => [step.id, step]))
  const inbound = inboundEvidence(input.pivot)
  const shape = shapeProbes(input.verdict, steps, input.recordings)
  const headers = headerProbes(input, steps, inbound)
  const probes = [
    ...shape,
    ...retransmissionProbes(input.pivot.deviations ?? [], input.recordings),
    ...suppressCrossFinalHeaders(shape, headers.probes)
  ]
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
  return { unackedFinals, steps: steps.map(documentStep) }
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
    inDialog: step.in_dialog ?? false
  }
}

/** Whether a recorded datagram is a BYE request. */
const isBye = (message: Bundle.RecordedMessage): boolean => {
  const line = startLineOf(message.raw)
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
    legsTornDownBySystem: legs.filter((leg) => leg.some((m) => m.dir === "in" && isBye(m))).length
  }
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
    at.probe.kind === "header"
      ? [at.probe.captured, at.probe.replayed]
      : shapeSides(at.probe.shapeKind)
  return {
    lane: meta.lane,
    capture: meta.capture,
    case: meta.case,
    run: meta.run,
    step: at.step,
    kind: at.probe.kind,
    signature: signature(at.probe),
    name: at.probe.kind === "header" ? at.probe.name : "",
    scope:
      at.probe.kind === "header"
        ? scopeText(at.probe.scope)
        : at.probe.scope === undefined
          ? ""
          : scopeText(at.probe.scope),
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
      const reference = capturedMessage(input.flows, step.observed)
      if (reference === undefined) {
        unreferenced += 1
        continue
      }
      compared += 1
      const scope = scopeOfRaw(message.raw)
      if (scope === undefined) continue
      const driven = drivenNames(driving, leg, message.at_us, scope)
      for (
        const probe of diffHeaders(
          headersInOrder(reference),
          headersInOrderRaw(message.raw),
          scope,
          inbound,
          driven,
          step.id
        )
      ) {
        probes.push({ step: step.id, probe })
      }
    }
  }
  return { probes, compared, unreferenced }
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
        scope: scopeOfRaw(message.raw),
        names: new Set(headersInOrderRaw(message.raw).map((h) => canonicalName(h.name)))
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
  flows: Flows.FlowsDoc | undefined,
  observed: Flow.Observed | undefined
): Flows.Msg | undefined => {
  if (flows === undefined || observed === undefined) return undefined
  return flows.legs[observed.leg]?.msgs[observed.msg]
}

/** The scope of a recorded reception, read off its own start line and To tag. */
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
 * One probe per header name whose two sides disagree under the name's fold.
 * First-appearance order, captured side first; a side that never states the
 * name is the empty occurrence list, and a name whose items are empty on both
 * sides states nothing on either.
 */
export const diffHeaders = (
  captured: ReadonlyArray<WireHeader>,
  replayed: ReadonlyArray<WireHeader>,
  scope: MsgScope,
  inbound: ReadonlyMap<string, ReadonlyArray<string>>,
  driven?: ReadonlySet<string>,
  step = ""
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
      driven: driven === undefined ? undefined : driven.has(key)
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
      const start = startLineOf(message.raw)
      if (start === undefined || start.kind !== "response") continue
      const cseq = (headerValuesOf(headersInOrderRaw(message.raw), "CSeq")[0] ?? "").trim()
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
 * record of that message: its response-scoped header probes hold two different
 * messages side by side and are dropped.
 */
const suppressCrossFinalHeaders = (
  shape: ReadonlyArray<ProbeAt>,
  headers: ReadonlyArray<ProbeAt>
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
  if (substituted.size === 0) return headers
  return headers.filter(
    (p) =>
      !(substituted.has(p.step) && p.probe.kind === "header" && p.probe.scope.kind === "response")
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
      const start = startLineOf(message.raw)
      if (start === undefined) continue
      const headers = headersInOrderRaw(message.raw)
      const callId = headerValuesOf(headers, "Call-ID")[0] ?? ""
      const cseq = (headerValuesOf(headers, "CSeq")[0] ?? "").trim()
      const [cseqNumber = "", cseqMethod = ""] = cseq.split(/\s+/)
      if (message.dir === "in" && start.kind === "response") {
        if (start.status < 200 || start.status >= 300 || cseqMethod.toUpperCase() !== "INVITE") continue
        const toTag = /;\s*tag\s*=\s*([^;]+)/i.exec(headerValuesOf(headers, "To")[0] ?? "")?.[1] ?? ""
        const key = `${callId} ${cseqNumber} ${toTag.trim()}`
        const list = finals.get(key) ?? []
        list.push(message.at_us)
        finals.set(key, list)
      }
      if (message.dir === "out" && start.kind === "request" && start.method === "ACK") {
        const key = `${callId} ${cseqNumber}`
        if (!acks.has(key)) acks.set(key, message.at_us)
      }
    }
    for (const [key, times] of finals) {
      const [callId = "", cseqNumber = ""] = key.split(" ")
      const ackAt = acks.get(`${callId} ${cseqNumber}`)
      count += times.slice(1).filter((at) => ackAt === undefined || at < ackAt).length
    }
  }
  return count
}

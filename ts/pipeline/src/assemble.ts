/**
 * CASE ASSEMBLY: one case spec becomes one pivot v3 document plus its body
 * resources.
 *
 * What the capture states is built here — the layout, the flow, the attempt
 * chain, the defect marker, the budgets. What one PLATFORM does is asked of the
 * {@link CasePolicy}: the family label, the lane verdicts, the handover causes,
 * the provisional profile, the lane's behavioural delta, the declared failure.
 *
 * Everything emitted is inside the generator subset: a capture states what
 * crossed the wire, and nothing that would need a human's judgement to justify.
 */
import {
  Flows,
  Pivot,
  type AllowedErrors,
  type Case,
  type Deviation,
  type Flow,
  type Placement
} from "@sip/contracts"
import type { ResourceFile } from "./bodies.js"
import type { CaseSpec } from "./case-spec.js"
import { buildCalls, CALLER_IDENTITY } from "./calls.js"
import { captureIndex, type CaptureIndex } from "./capture-index.js"
import { caseCallIds } from "./cut.js"
import { synthesize, type StepSource } from "./flowsteps.js"
import type { PartsIndex } from "./parts.js"
import type { Plan } from "./plan.js"
import type { CasePolicy, HitCoverage } from "./policy.js"
import type { SutSet } from "./sut.js"
import { build } from "./topology.js"
import { stampRfcViolations, unanchoredFlag } from "./violations.js"

export interface Assembled {
  readonly pivot: Pivot.PivotV3
  readonly resources: ReadonlyArray<ResourceFile>
  /** Registry entries this case matched but could not anchor — never silent. */
  readonly warnings: ReadonlyArray<string>
  /**
   * Every charged coordinate on this case's calls, answered: the ledger
   * `./caseset.ts` reads to decide whether a deferred refusal still stands.
   */
  readonly coverage: ReadonlyArray<HitCoverage>
}

/**
 * The margin the runner's own deadlines keep BEHIND the SIP deadline they wait
 * on: a budget equal to it makes the runner's give-up race the platform's, and
 * the run then books the ladder it was still riding instead of the teardown it
 * was waiting for.
 */
const DECISION_MARGIN_MS = 5_000

/** The slowest SIP deadline a scripted peer can be waiting behind: 64·T1. */
const SIP_DEADLINE_MS = 32_000

/** How long the runner may wait for one scripted expectation. */
const EXPECT_BUDGET_MS = SIP_DEADLINE_MS + DECISION_MARGIN_MS

/**
 * The settle ceiling: how long the runner may wait for every scripted dialog to
 * go terminal and the SUT's call count to reach zero. One SIP transaction
 * timeout's worth plus the margin, because the slowest legitimate settle is a
 * peer that never answers a teardown and has to time out.
 */
const SETTLE_BUDGET_MS = SIP_DEADLINE_MS + DECISION_MARGIN_MS

/**
 * A capture shows what crossed the wire; it shows nothing about what the system
 * billed. The gap is STATED with a reason token rather than guessed at, so every
 * generated case is greppable as one that still owes a CDR oracle.
 */
const NO_CDR_ORACLE = "capture-carries-no-cdr"

/**
 * The lane a captured document's asserted content came from: the platform the
 * packets were cut off, which is no replay lane. Every replay lane therefore
 * differs from it, which is what makes a classified check informative on all of
 * them (`PCAP2TEST_PIVOT_V3.md` §9.1).
 */
const CAPTURED_LANE = "captured-platform"

export interface AssembleInput {
  readonly flows: Flows.FlowsDoc
  readonly capture: string
  readonly spec: CaseSpec
  readonly sut: SutSet
  readonly plan: Plan
  readonly policy: CasePolicy
  readonly parts?: PartsIndex
  readonly allowed?: AllowedErrors.AllowedErrors
  /** The capture's once-built index; a caller assembling many cases passes one. */
  readonly index?: CaptureIndex
}

export const assemble = (input: AssembleInput): Assembled => {
  const { capture, flows, plan, policy, spec, sut } = input
  const index = input.index ?? captureIndex(flows, policy.derives, plan)
  const vantages = [spec.uac, ...spec.uas]
  const layout = build(
    flows,
    vantages,
    sut,
    plan,
    policy.derives,
    spec.chainHints,
    policy.joins,
    index
  )
  const flags: Array<Case.Flag> = [...layout.flags]

  // The capture first, then the lane it will be replayed on: synthesis states
  // what crossed the wire and the policy states the one behavioural delta
  // between the source platform and this one, flagging what it changed. The
  // policy's background readings reach both sides of §5.1: synthesis collapses
  // the claimed exchanges, the actors below carry the policies.
  const background = policy.background(layout)
  const flow = policy.adaptFlow(
    flows,
    synthesize(
      flows,
      layout,
      plan,
      policy.headerClass,
      input.parts ?? new Map(),
      background,
      policy.relay18x(flows, layout) === undefined,
      policy.relaysReinvite(flows, layout)
    ),
    layout
  )
  flags.push(...flow.flags)

  const actors: Array<Placement.Actor> = layout.actors.map((a) => ({
    id: a.id,
    kind: a.type,
    endpoint: a.endpoint,
    // The caller's identity is a registry entry; the actor names it.
    ...(a.type === "uac" ? { identity: CALLER_IDENTITY } : {}),
    // The claim's POSITION is not restated: `calls[].attempts[].leg` already
    // says which attempt this actor plays.
    ...(claimOf(a.claim?.by) ? { claim: { by: claimOf(a.claim?.by)! } } : {}),
    ...((background.get(a.id) ?? []).length > 0 ? { background: [...background.get(a.id)!] } : {})
  }))
  const callerLeg = layout.legs.find((l) =>
    actors.some((a) => a.id === l.actor && a.kind === "uac")
  )
  const called = buildCalls(
    flows,
    layout,
    flow.t0_us,
    callerLeg?.id ?? layout.legs[0]!.id,
    policy,
    flow.steps
  )
  flags.push(...called.flags)

  const defect = buildDefect(flows, spec, flow.sources, flags)
  const deviations = detectDeviations(flow.sources, defect?.marker.step)

  const legs = [...new Set(spec.cutLegs ?? vantages.map((v) => v.leg))].sort((a, b) => a - b)
  const callGroups = Flows.groupsForLegs(flows, legs)
  if (spec.sutNote !== undefined) {
    flags.push({ kind: "cut-sut-addresses", detail: spec.sutNote })
  }

  // What the allowed-errors registry already knows about this capture's calls.
  // A warning is a document annotation AND a caller-visible line: an entry that
  // matched but could not be anchored must not read as an absent violation.
  const callIds = caseCallIds(flows, sut, legs, index.correlation)
  const stamped = stampRfcViolations({
    registry: input.allowed,
    capture,
    flows,
    callIds,
    sources: flow.sources,
    steps: flow.steps,
    legs: layout.legs
  })
  for (const w of stamped.warnings) flags.push(unanchoredFlag(w))

  // What this case's replay CANNOT do compliantly. A negative case is generated,
  // never excused: the declaration rides the document and the run owes exactly
  // that failure (§11.2).
  const declared = policy.declare({
    capture,
    callIds,
    flows,
    layout,
    sources: flow.sources,
    steps: flow.steps
  })
  flags.push(...declared.flags)

  const endpoints: Array<Placement.Endpoint> = layout.endpoints.map((e) => ({
    id: e.id,
    observed: e.observed,
    side: "peer",
    binding: bindingOf(layout.actors, e.id)
  }))

  const attempts = called.calls.flatMap((c) => c.attempts)
  const view = {
    flags,
    actors,
    legs: layout.legs,
    steps: flow.steps,
    attempts,
    called: layout.topology.called
  }

  // The detector ROSTER, last, so it accounts for every reading above it. A
  // reader cannot otherwise tell a detector that ran and found nothing from one
  // that never ran, and absence means `none` and carries no other meaning.
  flags.push(...policy.detectors(view, called.calls))

  const pivot: Pivot.PivotV3 = {
    pivot_version: Pivot.PIVOT_VERSION,
    case: {
      id: spec.id,
      title: spec.title ?? defaultTitle(capture, spec, callGroups),
      family: policy.familyOf(view),
      variant: "repro",
      origin: "capture",
      origin_lane: CAPTURED_LANE,
      source: { capture, call_groups: callGroups, anonymized: true },
      ...(defect ? { defect } : {}),
      lanes: policy.lanes(view),
      ...(flags.length > 0 ? { annotations: { flags } } : {})
    },
    identities: called.identities,
    calls: called.calls,
    endpoints,
    actors,
    legs: layout.legs,
    flow: flow.steps,
    ...(deviations.length > 0 ? { deviations } : {}),
    ...(stamped.violations.length > 0 ? { rfc_violations: stamped.violations } : {}),
    ...(declared.entries.length > 0 ? { must_fail: declared.entries } : {}),
    postconditions: { cdr: { absent: NO_CDR_ORACLE } },
    timing: {
      expect_budget_ms: EXPECT_BUDGET_MS,
      settle_budget_ms: SETTLE_BUDGET_MS,
      // The furthest coordinate, not the last step's: a lane adaptation may move
      // a step, and the span is a fact about the CAPTURE. A step that states no
      // coordinate was not observed at all, so it spans nothing.
      capture_span_ms: Math.floor(
        Math.max(
          0,
          ...flow.steps.flatMap((s) => (s.observed === undefined ? [] : [s.observed.at_us]))
        ) / 1000
      )
    }
  }
  return {
    pivot,
    resources: flow.resources,
    warnings: stamped.warnings,
    coverage: declared.coverage
  }
}

/** The claim discriminator, where inference read one the contract names. */
const claimOf = (by: string | undefined): Placement.ClaimBy | undefined =>
  by === "ruri-pos" || by === "arrival-order" ? by : undefined

/**
 * An endpoint hosting both a UAC and a UAS is a loopback vantage — the callee
 * INVITE comes back to the caller's socket. The fact stays on the endpoint it
 * belongs to, so a case may mix the two.
 */
const bindingOf = (
  actors: ReadonlyArray<{ readonly endpoint: string; readonly type: string }>,
  endpoint: string
): Placement.Binding => {
  const kinds = new Set(actors.filter((a) => a.endpoint === endpoint).map((a) => a.type))
  return kinds.has("uac") && kinds.has("uas") ? "loopback" : "dedicated"
}

/**
 * The defect marker over the flow. Every captured message is a step, so a
 * coordinate can only miss by landing on a collapsed retransmission — which is
 * flagged, never silently dropped.
 */
const buildDefect = (
  flows: Flows.FlowsDoc,
  spec: CaseSpec,
  sources: ReadonlyArray<StepSource>,
  flags: Array<Case.Flag>
): Case.Defect | undefined => {
  if (spec.defects.length === 0) return undefined
  const mapped: Array<{ id: string; status?: number }> = []
  for (const [leg, msg] of spec.defects) {
    const legMsgs = flows.legs[leg]
    if (!legMsgs) throw new Error(`defect leg=${leg} msg=${msg}: leg ${leg} does not exist`)
    if (msg >= legMsgs.msgs.length) {
      throw new Error(`defect leg=${leg} msg=${msg}: message index out of range`)
    }
    const src = sources.find((s) => s.origLeg === leg && s.msgIdx === msg)
    if (src) {
      const summary = legMsgs.msgs[msg]!.summary
      mapped.push({
        id: src.id,
        ...(summary.kind === "response" ? { status: summary.status } : {})
      })
    } else {
      flags.push({
        kind: "defect-unmapped",
        detail:
          `defect marker leg=${leg} msg=${msg} fell on a retransmission collapsed into its ` +
          `first transmission — re-mark the first`
      })
    }
  }
  const responses = mapped.filter((m) => m.status !== undefined)
  const chosen =
    responses.length > 0
      ? responses.reduce((a, b) => (b.status! > a.status! ? b : a))
      : mapped[mapped.length - 1]
  if (!chosen) return undefined
  return {
    marker: { step: chosen.id },
    description: spec.defectNote ?? "observed defect (see the marked step)"
  }
}

/**
 * `verbatim-emission` on the send that triggers the defect. The trigger is
 * sought among SCRIPTED sends only: an `auto` step is the interpreter stack's
 * own composition, so preserving its header order is not this document's to ask.
 */
const detectDeviations = (
  sources: ReadonlyArray<StepSource>,
  defectStep: string | undefined
): Array<Deviation.Deviation> => {
  if (defectStep === undefined) return []
  const order = new Map(sources.map((s, i) => [s.id, i]))
  const defectAt = order.get(defectStep)
  if (defectAt === undefined) return []
  const defectSrc = sources[defectAt]!
  const trigger = sources
    .filter(
      (s) => s.pivotLeg === defectSrc.pivotLeg && s.emits && !s.auto && order.get(s.id)! < defectAt
    )
    .reduce<StepSource | undefined>(
      (a, b) => (a === undefined || order.get(b.id)! > order.get(a.id)! ? b : a),
      undefined
    )
  if (!trigger) return []
  return [
    {
      id: "d1",
      kind: "verbatim-emission",
      leg: trigger.pivotLeg,
      step: trigger.id,
      preserve: ["header-order", "casing"]
    }
  ]
}

const defaultTitle = (
  capture: string,
  spec: CaseSpec,
  callGroups: ReadonlyArray<number>
): string => spec.defectNote ?? `${capture} basic call (groups ${callGroups.join("+")})`

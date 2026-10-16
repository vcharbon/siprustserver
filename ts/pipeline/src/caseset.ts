/**
 * WHICH cases a capture yields, and what refused the rest.
 *
 * The cut proposes a case per call (`./cut.ts`); this file decides whether each
 * one is written. The ORDER is the whole of it:
 *
 * 1. every refusal that needs no document is decided first — the CAPTURE tier
 *    (the family alone) ahead of the CASE tier (the family plus the cut's
 *    vantages, the pipeline's own rules ahead of the policy's), so a rule never
 *    sees more than its decision needs — and such a case is never assembled;
 * 2. what is left is a DEFERRED refusal — the case is assembled, and the refusal
 *    is withdrawn exactly where the document DECLARES every charged coordinate
 *    it names;
 * 3. the assembled document is read for the refusals only a document can state —
 *    the pipeline's own (`./document-rules.ts`) ahead of the deployment's, and
 *    every rule that fires is recorded, because refusals are all-that-apply.
 *    What the document already DECLARES rides along, so a hole the declaration
 *    lane names is stated as a negative case rather than refused twice.
 *
 * Only a coordinate nothing anchors keeps the refusal, because a case that
 * cannot state its own divergence would replay one it never declared. A
 * deferral is a claim about the case's document, so a case refused before
 * assembly records none: no document was built to evaluate it against. One
 * evaluated on the document and still standing is recorded beside the
 * document's own refusals, all-that-apply.
 *
 * Deciding is separated from writing on purpose: a generator writes the cases
 * and a registry sync only READS what the generator would do with each hit, and
 * both must answer identically or the registry and the case tree drift apart.
 *
 * QUARANTINE is the refusal lane's falsifier and nothing else: under it, a
 * refused case is assembled anyway and the document rides {@link
 * CaseOutcome.quarantined} so a caller can replay what the refusal deleted,
 * unless an `unrunnable` document rule says no run of it can end or an
 * `untrusted` refusal says no run of it speaks to the call. It never
 * changes which cases are generated — `built` and `refused` answer
 * identically with it on and off — because a falsifier that moved the corpus
 * would be measuring itself.
 */
import type { AllowedErrors, Flows } from "@sip/contracts"
import { assemble, type Assembled } from "./assemble.js"
import { captureIndex, type CaptureIndex } from "./capture-index.js"
import type { CaseSpec } from "./case-spec.js"
import { caseCallIds } from "./cut.js"
import type { PartsIndex } from "./parts.js"
import type { Plan } from "./plan.js"
import { withdrawnBy, type CasePolicy, type Refusal } from "./policy.js"
import { CAPTURE_RULES, type CaptureInput, type CaptureRule } from "./capture-rules.js"
import { CASE_RULES, type CaseRule, type RefusalInput } from "./case-rules.js"
import { DOCUMENT_RULES, type DocumentInput, type DocumentRule } from "./document-rules.js"
import { refusalOf, type RefusalSite } from "./refusal-rule.js"
import type { SutSet } from "./sut.js"

/** One proposed case, decided. */
export interface CaseOutcome {
  readonly spec: CaseSpec
  /** The case's own legs: `cutLegs`, else the vantages' — what its refusals are recorded against. */
  readonly legs: ReadonlyArray<number>
  /** The case's own correlated calls (`cut.ts::caseCallIds`). */
  readonly callIds: ReadonlyArray<string>
  /** The assembled document, where the case is generated. */
  readonly built?: Assembled
  /** What refused it. Empty exactly where the case is generated. */
  readonly refused: ReadonlyArray<Refusal>
  /** The assembly's own failure, where it threw: no document, and no silence. */
  readonly error?: string
  /**
   * What the refusal deleted, assembled anyway — only under `quarantine`, only
   * on a refused case. A caller replays this to falsify the refusal.
   */
  readonly quarantined?: Assembled
  /** Why there is no quarantined document: assembly threw. */
  readonly quarantineError?: string
}

/** What a capture yields, in the order a report states it. */
export interface CaptureCases {
  readonly outcomes: ReadonlyArray<CaseOutcome>
  /** Every refusal, across every proposed case. */
  readonly refused: ReadonlyArray<Refusal>
}

export interface CaseSetInput {
  readonly flows: Flows.FlowsDoc
  readonly capture: string
  readonly specs: ReadonlyArray<CaseSpec>
  readonly sut: SutSet
  readonly plan: Plan
  readonly policy: CasePolicy
  readonly parts?: PartsIndex
  readonly allowed?: AllowedErrors.AllowedErrors
  /** Assemble refused cases too, and carry their documents. Never changes a verdict. */
  readonly quarantine?: boolean
}

/** One spec assembled, or the reason it threw. Never both, never neither. */
const build = (
  input: CaseSetInput,
  index: CaptureIndex,
  spec: CaseSpec
): { readonly built: Assembled } | { readonly error: string } => {
  try {
    return {
      built: assemble({
        flows: input.flows,
        capture: input.capture,
        spec,
        sut: input.sut,
        plan: input.plan,
        policy: input.policy,
        index,
        ...(input.parts === undefined ? {} : { parts: input.parts }),
        ...(input.allowed === undefined ? {} : { allowed: input.allowed })
      })
    }
  } catch (e) {
    return { error: String(e) }
  }
}

/**
 * What a refused case carries under quarantine: the document the refusal
 * deleted, or the reason there is none. A case an `untrusted` refusal drops
 * has none — a run of a document the capture cannot be trusted to state
 * replays no call the wire carried — and neither has a document an
 * `unrunnable` rule fires on — no run of it can be driven to its end; either
 * rule's own line says why. Nothing under quarantine off.
 */
const quarantineOf = (
  input: CaseSetInput,
  index: CaptureIndex,
  spec: CaseSpec,
  refused: ReadonlyArray<Refusal>,
  already?: Assembled
): Pick<CaseOutcome, "quarantined" | "quarantineError"> => {
  if (input.quarantine !== true) return {}
  const untrusted = refused.find((r) => r.disposition === "untrusted")
  if (untrusted !== undefined) return { quarantineError: untrusted.line }
  const attempt = already === undefined ? build(input, index, spec) : { built: already }
  if (!("built" in attempt)) return { quarantineError: attempt.error }
  const unrunnable = [...DOCUMENT_RULES, ...input.policy.refuseOnDocument]
    .filter((rule) => rule.disposition === "unrunnable")
  const why = onDocument(unrunnable, { caseId: spec.id, capture: input.capture }, documentInput(input, spec, attempt.built))[0]
  return why === undefined ? { quarantined: attempt.built } : { quarantineError: why.line }
}

/** What the case-document tier reads off one assembled document. */
const documentInput = (input: CaseSetInput, spec: CaseSpec, built: Assembled): DocumentInput => ({
  capture: input.capture,
  caseId: spec.id,
  flow: built.pivot.flow,
  calls: built.pivot.calls,
  declared: built.pivot.must_fail ?? []
})

/**
 * Every rule of one roster that fires, recorded against the case being decided.
 * One dispatch per TIER, because each takes a different input and nothing here
 * is allowed to be generic over which.
 *
 * ALL-THAT-APPLY, never first-match-wins: a case two rules refuse is refused
 * twice, under both tokens, because each is a separate claim a replay can
 * falsify on its own. The order of the answer is the roster's own, which is the
 * order the records take on disk.
 */
const atCapture = (
  rules: ReadonlyArray<CaptureRule>,
  site: RefusalSite,
  input: CaptureInput
): ReadonlyArray<Refusal> =>
  rules.flatMap((rule) => {
    const finding = rule.refuses(input)
    return finding === undefined ? [] : [refusalOf(rule, site, finding)]
  })

/** Every case-tier rule that fires, in roster order. */
const atCase = (
  rules: ReadonlyArray<CaseRule>,
  site: RefusalSite,
  input: RefusalInput
): ReadonlyArray<Refusal> =>
  rules.flatMap((rule) => {
    const finding = rule.refuses(input)
    return finding === undefined ? [] : [refusalOf(rule, site, finding)]
  })

/** Every case-document-tier rule that fires, in roster order. */
const onDocument = (
  rules: ReadonlyArray<DocumentRule>,
  site: RefusalSite,
  input: DocumentInput
): ReadonlyArray<Refusal> =>
  rules.flatMap((rule) => {
    const finding = rule.refuses(input)
    return finding === undefined ? [] : [refusalOf(rule, site, finding)]
  })

export const decideCases = (input: CaseSetInput): CaptureCases => {
  const outcomes: Array<CaseOutcome> = []
  // Once per capture, whatever the number of cases: the correlation and the
  // forms table are each a pass over the whole document.
  const index = captureIndex(input.flows, input.policy.derives, input.plan)
  for (const spec of input.specs) {
    const legs = spec.cutLegs ?? [spec.uac.leg, ...spec.uas.map((u) => u.leg)]
    const callIds = caseCallIds(input.flows, input.sut, legs, index.correlation)
    // The capture tier first, because it needs strictly less: a rule that
    // decides on the family alone never sees the vantages the cut produced.
    const captured = {
      flows: input.flows,
      capture: input.capture,
      sut: input.sut,
      legs,
      callIds,
      caseId: spec.id
    }
    const site = { caseId: spec.id, capture: input.capture }
    const refusals = [
      ...atCapture([...CAPTURE_RULES, ...input.policy.refuseAtCapture], site, captured),
      ...atCase([...CASE_RULES, ...input.policy.refuse], site, { ...captured, spec })
    ]

    // A case refused on a rule no declaration can answer is never assembled,
    // so its deferrals, claims about a document, are not recorded.
    if (refusals.some((r) => r.disposition !== "defers")) {
      const refused = refusals.filter((r) => r.disposition !== "defers")
      outcomes.push({ spec, legs, callIds, refused, ...quarantineOf(input, index, spec, refused) })
      continue
    }
    const attempt = build(input, index, spec)
    if ("error" in attempt) {
      // A case only deferrals refuse, with no document: nothing is declared,
      // so every deferral stands.
      outcomes.push({
        spec,
        legs,
        callIds,
        refused: refusals,
        error: attempt.error,
        ...(input.quarantine === true ? { quarantineError: attempt.error } : {})
      })
      continue
    }
    const built = attempt.built
    // The pipeline's own document rules first, then the deployment's: the
    // ORDER is the on-disk order, and every rule that fires is recorded.
    const standing = [
      ...refusals.filter((r) => !withdrawnBy(r, built.coverage)),
      ...onDocument([...DOCUMENT_RULES, ...input.policy.refuseOnDocument], site, documentInput(input, spec, built))
    ]
    outcomes.push(
      standing.length > 0
        ? { spec, legs, callIds, refused: standing, ...quarantineOf(input, index, spec, standing, built) }
        : { spec, legs, callIds, built, refused: [] }
    )
  }
  return { outcomes, refused: outcomes.flatMap((o) => o.refused) }
}

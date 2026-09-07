/**
 * WHICH cases a capture yields, and what refused the rest.
 *
 * The cut proposes a case per call (`./cut.ts`); this file decides whether each
 * one is written. The ORDER is the whole of it:
 *
 * 1. every refusal the policy states that needs no document is decided first —
 *    the CAPTURE tier (the family alone) ahead of the CASE tier (the family
 *    plus the cut's vantages), so a rule never sees more than its decision
 *    needs — and such a case is never assembled;
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
 * cannot state its own divergence would replay one it never declared.
 *
 * Deciding is separated from writing on purpose: a generator writes the cases
 * and a registry sync only READS what the generator would do with each hit, and
 * both must answer identically or the registry and the case tree drift apart.
 *
 * QUARANTINE is the refusal lane's falsifier and nothing else: under it, a
 * refused case is assembled anyway and the document rides {@link
 * CaseOutcome.quarantined} so a caller can replay what the refusal deleted. It
 * never changes which cases are generated — `built` and `refused` answer
 * identically with it on and off — because a falsifier that moved the corpus
 * would be measuring itself.
 */
import type { AllowedErrors, Flows } from "@sip/contracts"
import { assemble, type Assembled } from "./assemble.js"
import type { CaseSpec } from "./case-spec.js"
import { caseCallIds } from "./cut.js"
import type { PartsIndex } from "./parts.js"
import type { Plan } from "./plan.js"
import { withdrawnBy, type CasePolicy, type Refusal } from "./policy.js"
import type { CaptureInput, CaptureRule } from "./capture-rules.js"
import type { CaseRule, RefusalInput } from "./case-rules.js"
import { DOCUMENT_RULES, type DocumentInput, type DocumentRule } from "./document-rules.js"
import { refusalOf, type RefusalSite } from "./refusal-rule.js"
import type { SutSet } from "./sut.js"

/** One proposed case, decided. */
export interface CaseOutcome {
  readonly spec: CaseSpec
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
 * deleted, or the reason there is none. Nothing under quarantine off.
 */
const quarantineOf = (
  input: CaseSetInput,
  spec: CaseSpec,
  already?: Assembled
): Pick<CaseOutcome, "quarantined" | "quarantineError"> => {
  if (input.quarantine !== true) return {}
  if (already !== undefined) return { quarantined: already }
  const attempt = build(input, spec)
  return "built" in attempt ? { quarantined: attempt.built } : { quarantineError: attempt.error }
}

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
  for (const spec of input.specs) {
    const legs = spec.cutLegs ?? [spec.uac.leg, ...spec.uas.map((u) => u.leg)]
    const callIds = caseCallIds(input.flows, input.sut, legs, input.policy.derives)
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
      ...atCapture(input.policy.refuseAtCapture, site, captured),
      ...atCase(input.policy.refuse, site, { ...captured, spec })
    ]

    // A case refused on a rule no declaration can answer is never assembled, so
    // its deferred refusals stand with it: there is no document to declare in.
    if (refusals.some((r) => r.disposition !== "defers")) {
      outcomes.push({ spec, callIds, refused: refusals, ...quarantineOf(input, spec) })
      continue
    }
    const attempt = build(input, spec)
    if ("error" in attempt) {
      // No document, so nothing is declared and every deferred refusal stands.
      outcomes.push({
        spec,
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
      ...onDocument([...DOCUMENT_RULES, ...input.policy.refuseOnDocument], site, {
        capture: input.capture,
        caseId: spec.id,
        flow: built.pivot.flow,
        declared: built.pivot.must_fail ?? []
      })
    ]
    outcomes.push(
      standing.length > 0
        ? { spec, callIds, refused: standing, ...quarantineOf(input, spec, built) }
        : { spec, callIds, built, refused: [] }
    )
  }
  return { outcomes, refused: outcomes.flatMap((o) => o.refused) }
}

/**
 * The CASE POLICY: every reading a deployment owns, gathered in one record so
 * the assembly path can be read top to bottom without knowing whose platform it
 * is running against.
 *
 * The engine below this file decides what CROSSED THE WIRE. A capture shows the
 * bytes and nothing else, so everything that needs to know how one platform
 * behaves — which Call-IDs it derives, why it moved to the next attempt, how it
 * handles provisionals, whether it ACKs locally, which lanes can replay the
 * result, which failure a run owes and which case is refused outright — arrives
 * here as a function.
 *
 * {@link neutralPolicy} is what an unconfigured pipeline runs: each default
 * STATES NOTHING rather than guessing. A guess is worse than silence, because a
 * generated document that names a cause nobody detected reads exactly like one
 * that observed it.
 *
 * Extension is Layer substitution and nothing else: a deployment builds one of
 * these and hands it to {@link CaseAssembler}.
 */
import type { Call, Case, Flow, Flows, MustFail, Placement, Tokens } from "@sip/contracts"
import type { BackgroundMap } from "./background.js"
import type { CaptureRule } from "./capture-rules.js"
import type { CaseRule } from "./case-rules.js"
import { CUT_RULES } from "./cut.js"
import { neverDerives, type CallIdDerivation } from "./derivation.js"
import { DOCUMENT_RULES, type DocumentRule } from "./document-rules.js"
import { NO_HEADER_CLASS, type FlowOut, type HeaderClassifier, type StepSource } from "./flowsteps.js"
import { checkRefusalRoster } from "./refusal-roster.js"
import type { RuleIdentity } from "./refusal-rule.js"
import type { ActorObs, JoinsReading, Layout, PartyIdentity } from "./topology.js"

/** The family label a document carries when nothing classified it. */
export const UNCLASSIFIED = "unclassified"

/**
 * Why one attempt handed over to the next, at the topology position it left.
 * The evidence is the reading's own, in the words a human confirms it by.
 */
export interface CauseReading {
  /** The position the handover left, in `called[<branch>][<position>]` form. */
  readonly from: string
  readonly cause: Tokens.Cause
  readonly evidence: ReadonlyArray<string>
  /** The dwell the platform's own no-answer timer ran, where it ran one. */
  readonly after_ms?: number
}

/** Every handover of one case's attempt chain, and what the reading is unsure of. */
export interface ChainReading {
  readonly causes: ReadonlyArray<CauseReading>
  readonly flags: ReadonlyArray<Case.Flag>
}


/**
 * The case as the generic passes leave it: what a per-deployment label or lane
 * verdict is read off. One view for both, so two readings of the same case
 * cannot disagree about what they were shown.
 */
export interface CaseView {
  readonly flags: ReadonlyArray<Case.Flag>
  readonly actors: ReadonlyArray<Placement.Actor>
  readonly legs: ReadonlyArray<Placement.Leg>
  readonly steps: ReadonlyArray<Flow.Step>
  readonly attempts: ReadonlyArray<Call.Attempt>
  /** The called parties per branch and position, as topology inference read them. */
  readonly called: ReadonlyArray<ReadonlyArray<PartyIdentity>>
}

/** What a declaration is derived from: the case's own flow, at its own vantage. */
export interface DeclarationInput {
  /**
   * The topology the case was cut on. A declaration is MECHANISM-CONDITIONAL —
   * which answer a platform mints itself rather than relaying depends on the
   * call's shape, which the flow alone does not carry.
   */
  readonly layout: Layout
  /** The source capture's file name, as a deployment's own records name it. */
  readonly capture: string
  /** The Call-IDs of the case's own correlated calls (`cut.ts::caseCallIds`). */
  readonly callIds: ReadonlyArray<string>
  readonly flows: Flows.FlowsDoc
  readonly sources: ReadonlyArray<StepSource>
  readonly steps: ReadonlyArray<Flow.Step>
}

/**
 * One charged coordinate a refusal rests on. A declaration ANSWERS a refusal by
 * anchoring every one of these on a step; a coordinate nothing anchors is what
 * keeps the refusal standing.
 */
export interface ChargedHit {
  /** The census rule that charged it — two rules on one coordinate stay two questions. */
  readonly rule: string
  readonly callId: string
  /** The charged endpoint, `ip:port`. */
  readonly emitter: string
  /** Index into the swept document's `legs`. */
  readonly leg: number
}

/** How this case answered one charged coordinate. Exactly one of the two is set. */
export interface HitCoverage extends ChargedHit {
  /** The step id that declares it. */
  readonly anchor?: string
  /** Why there is none. */
  readonly refusal?: string
}

/** What a declaration derived: the entries, the evidence, and the ledger. */
export interface Declared {
  readonly entries: ReadonlyArray<MustFail.MustFail>
  /** One flag per declaration: the evidence, in the detector's own words. */
  readonly flags: ReadonlyArray<Case.Flag>
  /** Every charged coordinate on this case's calls, answered or not. */
  readonly coverage: ReadonlyArray<HitCoverage>
}

/** One datagram, leg or group a refusal rests on. Flows coordinates only. */
export interface RefusalEvidence {
  readonly group?: number
  readonly leg?: number
  readonly hop?: number
  readonly msg?: number
  readonly callId?: string
  /** The endpoint a hit was charged against, `ip:port`. */
  readonly emitter?: string
  /** The rule that charged it, where one did. */
  readonly rule?: string
  /** What was seen, in one clause. */
  readonly detail: string
}

/**
 * What a refusal does to the case, beyond not generating it.
 *
 * - `refuses` — decided, final, and a replay could contradict it.
 * - `defers` — waits on the case's own document: refused only where the case
 *   cannot DECLARE the divergence it names, because a case that replays a
 *   divergence it never declared is worse than no case at all.
 * - `not-proposed` — no case was ever on the table. The family is not a call.
 * - `unreachable` — no document can exist for it, so no replay can falsify it.
 *   A DECLARED zero, never a silent one.
 */
export type Disposition = "refuses" | "defers" | "not-proposed" | "unreachable"

/**
 * One reason a proposed case is not generated, in the one shape every tier
 * records — the cut's own included, so a refusal decided before any case spec
 * exists is enumerable, quarantinable and falsifiable beside the rest.
 */
export interface Refusal {
  readonly caseId: string
  readonly capture: string
  /** Open token naming the rule that refused it. */
  readonly reason: string
  /** The one-line finding, for stderr. */
  readonly line: string
  readonly disposition: Disposition
  /** The captured legs it rests on, where the rule names them. */
  readonly legs?: ReadonlyArray<number>
  /** The flows groups it rests on, where the rule names them. */
  readonly groups?: ReadonlyArray<number>
  /**
   * The Call-IDs the rule itself charged. ABSENT where the rule charges the
   * whole case — the recorder fills the case's own in, so the two never share a
   * key while meaning different things.
   */
  readonly callIds?: ReadonlyArray<string>
  /** What it rests on, down to the datagram. */
  readonly evidence?: ReadonlyArray<RefusalEvidence>
  /** The coordinates a declaration must anchor to withdraw it. */
  readonly charged?: ReadonlyArray<ChargedHit>
}

/** Whether the case's declarations answered every coordinate `refusal` rests on. */
export const withdrawnBy = (
  refusal: Refusal,
  coverage: ReadonlyArray<HitCoverage>
): boolean =>
  refusal.disposition === "defers" &&
  (refusal.charged ?? []).every((hit) =>
    coverage.some(
      (c) =>
        c.rule === hit.rule &&
        c.callId === hit.callId &&
        c.emitter === hit.emitter &&
        c.leg === hit.leg &&
        c.anchor !== undefined
    )
  )

/** Every reading a deployment owns, as one substitutable value. */
export interface CasePolicy {
  /** Whether one leg's Call-ID is the one this platform minted off another's. */
  readonly derives: CallIdDerivation
  /** Why each attempt of the chain handed over to the next. */
  readonly chain: (flows: Flows.FlowsDoc, layout: Layout) => ChainReading
  /** The provisional-handling profile the captured platform ran. */
  readonly relay18x: (flows: Flows.FlowsDoc, layout: Layout) => Call.Relay18x | undefined
  /**
   * Whether the REPLAYING platform relays an in-dialog INVITE end to end. Where
   * it does, a leg the vantage lost past the 2xx its peer sent is owed the far
   * side of every such exchange the other leg holds, and synthesis transcribes
   * it there (`far-side-reinvite.ts`). Neutral: states nothing, so nothing is
   * derived and the run shows what the platform does with the INVITE.
   */
  readonly relaysReinvite: (flows: Flows.FlowsDoc, layout: Layout) => boolean
  /**
   * Which called legs are resources the platform JOINED to the call rather than
   * destinations it hunted — read off the raw observations, since a media
   * resource is told apart by what its dialog carries, not by its position.
   */
  readonly joins: (
    flows: Flows.FlowsDoc,
    actors: ReadonlyArray<ActorObs>
  ) => JoinsReading
  /**
   * Any behavioural delta between the source platform and the replaying one,
   * applied to the synthesized flow. A pure rewrite: what it changes it flags,
   * and a deployment whose two platforms behave alike supplies the identity.
   *
   * The layout rides along because such a delta can be MECHANISM-CONDITIONAL:
   * what a platform composes itself rather than relaying may be decided by the
   * call's shape (a chain, a transfer, a joined resource), which the flow alone
   * does not carry.
   */
  readonly adaptFlow: (flows: Flows.FlowsDoc, flow: FlowOut, layout: Layout) => FlowOut
  /**
   * The §9.1 class an ASSERTED frozen header of this name carries, where the
   * deployment owns its vocabulary (`origin-platform-header` families).
   */
  readonly headerClass: HeaderClassifier
  /** The informative family label. Never interpreted by anything downstream. */
  readonly familyOf: (view: CaseView) => string
  /**
   * One flag per DETECTOR this deployment runs, stating its outcome on this
   * case (`detected:` / `detected-none:` / `detection-unavailable:`).
   *
   * The roster is complete or it is worthless: a silent detector and an absent
   * shape read identically, which is how a mis-cut family goes unnoticed. Lint
   * refuses a captured document missing a roster entry.
   */
  readonly detectors: (
    view: CaseView,
    calls: ReadonlyArray<Call.Call>
  ) => ReadonlyArray<Case.Flag>
  /** On which lanes the document is replayable, and why not. */
  readonly lanes: (view: CaseView) => Record<string, Tokens.LaneVerdict>
  /** The failure this case's run MUST produce, derived rather than guessed. */
  readonly declare: (input: DeclarationInput) => Declared
  /**
   * Why a call family yields no case, decided on the family alone. Evaluated
   * before {@link CasePolicy.refuse} because it needs strictly less.
   */
  readonly refuseAtCapture: ReadonlyArray<CaptureRule>
  /** Why a proposed case is not generated, decided on the cut's vantages. */
  readonly refuse: ReadonlyArray<CaseRule>
  /**
   * Why an ASSEMBLED case is not written, decided on the document alone. Runs
   * after the pipeline's own document rules, which every deployment gets.
   */
  readonly refuseOnDocument: ReadonlyArray<DocumentRule>
  /**
   * The background policies each actor states (§5.1), actor id → policies. A
   * stated policy does double duty: the assembled actor carries it, and
   * synthesis collapses the captured platform's own locally-minted exchanges
   * of a policy-answered method into it instead of scripting them
   * (`background.ts`). Neutral: no actor states any.
   */
  readonly background: (layout: Layout) => BackgroundMap
}

/**
 * The policy of a pipeline no deployment has configured: nothing derives,
 * nothing is caused, nothing is adapted, nothing is refused.
 *
 * A case assembled under it is a faithful transcription of the capture at its
 * vantage and carries no claim about the system that produced it — which is
 * exactly what an unconfigured run is entitled to say.
 */
export const neutralPolicy: CasePolicy = {
  derives: neverDerives,
  chain: () => ({ causes: [], flags: [] }),
  relay18x: () => undefined,
  relaysReinvite: () => false,
  joins: () => new Map(),
  adaptFlow: (_flows, flow) => flow,
  headerClass: NO_HEADER_CLASS,
  familyOf: () => UNCLASSIFIED,
  detectors: () => [],
  lanes: () => ({}),
  declare: () => ({ entries: [], flags: [], coverage: [] }),
  refuseAtCapture: [],
  refuse: [],
  refuseOnDocument: [],
  background: () => new Map()
}

/**
 * A policy with some readings replaced; every unstated one stays neutral.
 *
 * The refusal roster is checked HERE, where the deployment's rules first meet
 * the pipeline's own: a token used at two tiers, or a subject a token
 * contradicts, is a load-time error and not a corpus one.
 */
export const policyWith = (overrides: Partial<CasePolicy>): CasePolicy => {
  const policy: CasePolicy = { ...neutralPolicy, ...overrides }
  checkRefusalRoster(refusalRoster(policy), { where: "the case policy's refusal roster" })
  return policy
}

/**
 * The refusal roster in its TIERS, each rule with its predicate: what a reader
 * needs to EVALUATE a rule rather than only name it.
 *
 * Three arrays and no union, because the input type IS the tier — a shape
 * generic over it would let a caller hand a rule evidence its tier does not
 * hold, which is the boundary `./capture-rules.ts`, `./case-rules.ts` and
 * `./document-rules.ts` exist to draw.
 */
export interface TieredRoster {
  readonly capture: ReadonlyArray<CaptureRule>
  readonly case: ReadonlyArray<CaseRule>
  readonly document: ReadonlyArray<DocumentRule>
}

/** Every refusal rule a policy runs, by tier, in the order a capture meets them. */
export const tieredRoster = (policy: CasePolicy): TieredRoster => ({
  capture: [...CUT_RULES, ...policy.refuseAtCapture],
  case: policy.refuse,
  document: [...DOCUMENT_RULES, ...policy.refuseOnDocument]
})

/**
 * Every refusal rule a policy runs, both packages' own, in the order a capture
 * meets them. The roster is the whole answer to "what refuses a case" — a
 * reader that has to gather it from four files can only report the rules it
 * happened to find, which is how a rule goes years without being measured.
 *
 * Derived from {@link tieredRoster} so the names and the predicates can never
 * be two different lists.
 */
export const refusalRoster = (policy: CasePolicy): ReadonlyArray<RuleIdentity> => {
  const tiers = tieredRoster(policy)
  return [...tiers.capture, ...tiers.case, ...tiers.document]
}

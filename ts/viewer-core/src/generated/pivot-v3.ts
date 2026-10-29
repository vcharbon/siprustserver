/* eslint-disable */
/**
 * DO NOT EDIT. Generated from the Rust-owned JSON Schema by `pnpm regen-types`.
 * Source of truth: `pivot-schema schema pivot` (crate `pivot-schema`).
 * Hand edits are lost on the next regeneration.
 */

/**
 * Why the platform left an attempt: `no-answer`, `no-provisional`, `busy`, `transaction-timeout`, `closed:bye`, `redirect:<3xx>` or `external:<4xx-6xx>`.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Cause".
 */
export type Cause = string
/**
 * Lane verdict: `ok` or `blocked:<reason>`.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "LaneVerdict".
 */
export type LaneVerdict = string
/**
 * A `cseq-override`'s value.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CseqValue".
 */
export type CseqValue = number | Computed
/**
 * One flow node, discriminated by `op`: a message step, an `inject`, an `alt` or an `unordered` group.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "FlowNode".
 */
export type FlowNode = Step | Inject | Alt | Unordered
/**
 * What an `expect` does with the content it stored. The GENERATOR decides and
 * the document says which; a lane never picks the meaning. Under both values
 * the step still gates on `op`, leg alignment, the discriminator and
 * `within_ms`.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CheckMode".
 */
export type CheckMode = "assert" | "record"
/**
 * What vocabulary an assertion reads. Closed: a class is a promise that one
 * named downgrade rule applies to it, and a rule nothing implements is not a
 * promise.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CheckClass".
 */
export type CheckClass = "origin-platform-header" | "cdr-vocabulary"
/**
 * The body a step emits or checks.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Body".
 */
export type Body = MultipartBody | ResourceBody | ShapeBody
/**
 * How a received body is compared with the resource an expect states.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BodyCompare".
 */
export type BodyCompare = "exact" | "xml" | "sdp"
/**
 * How the registry handles a stored body or part: `frozen` replays and
 * compares byte-exact whatever the payload holds, except that an MSCML
 * `<response>` a send emits takes the client-defined id of the request it
 * answers (RFC 5022 §10.2, §8.3). Whether the payload is text or bytes is a
 * fact of the bytes, never a declared mode (ADR-0035).
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BodyMode".
 */
export type BodyMode = "frozen"
/**
 * A tier-2 reference: either the numbering plan recognized the value and the
 * document stores its ROLE, or it did not and the document freezes the value.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Ref".
 */
export type Ref = PositionalRef | FrozenRef
/**
 * What the run must have billed, or why nothing can be said about it.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CdrExpectation".
 */
export type CdrExpectation = CdrCheck | CdrAbsent
/**
 * A run-time value: `${leg:<id>.<field>}`, `${early:<id>.<field>}`, `${step:<id>.<field>}` or `${num:<identity>:<form>}`.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Accessor".
 */
export type Accessor = string
/**
 * The SIP role an actor plays.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "ActorKind".
 */
export type ActorKind = "uac" | "uas" | "mrf"
/**
 * The `op` of an `alt` node.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "AltOp".
 */
export type AltOp = "alt"
/**
 * Delay anchor: `trigger` or `step:<id>`.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Anchor".
 */
export type Anchor = string
/**
 * How the lane must bind an endpoint. Stated PER ENDPOINT: a case may mix a
 * loopback vantage on one attempt with a dedicated one on the next.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Binding".
 */
export type Binding = "dedicated" | "loopback"
/**
 * The declared shape of an expected body.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BodyShape".
 */
export type BodyShape = "sdp-present" | "absent" | "multipart-present"
/**
 * The assertion operator.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CheckOp".
 */
export type CheckOp = "eq" | "regex" | "exists" | "absent"
/**
 * The claim discriminator.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "ClaimBy".
 */
export type ClaimBy = "ruri-pos" | "arrival-order"
/**
 * The failures this format can declare. Closed: each member names a failure
 * the generator can predict from a detector's hit plus this lane's behaviour.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "DeclaredFailure".
 */
export type DeclaredFailure = "unexpected-ack" | "unexpected-prack" | "unexpected-cancel"
/**
 * Which side of the dialog the actor is on.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Direction".
 */
export type Direction = "out" | "in"
/**
 * The `op` of an `inject` node.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "InjectOp".
 */
export type InjectOp = "inject"
/**
 * The mechanisms that join a leg to a running call. Closed: each names a SIP
 * event the document itself carries as a step, and a mechanism nothing can
 * point at is not a join.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "JoinKind".
 */
export type JoinKind = "refer" | "info" | "mrf"
/**
 * Whether the actor emits the message or waits for it.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Op".
 */
export type Op = "send" | "expect"
/**
 * How the document came to exist. The two are not styles of one thing: a
 * captured document is a projection of packets a tool saw, and everything it
 * carries must be readable off them.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Origin".
 */
export type Origin = "capture" | "authored"
/**
 * A rule of the RFC validator, by its wire token.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "RfcRule".
 */
export type RfcRule =
  | "ack-body-after-complete-offer-answer"
  | "ack-cseq-matches-invite"
  | "ack-preserves-invite-route"
  | "ack-require-subset-of-invite"
  | "allow-supported-on-invite"
  | "answer-m-line-count-matches-offer"
  | "answer-media-type-matches-offer"
  | "answer-stream-matches-offer"
  | "answer-t-line-equals-offer"
  | "branch-prefix"
  | "c0-port-non-zero"
  | "cancel-after-1xx"
  | "cancel-cseq-method"
  | "cancel-request-uri"
  | "cancel-route-echoes-invite"
  | "cancel-via-branch"
  | "concurrent-re-invite-500-or-491"
  | "contact-presence"
  | "content-length"
  | "content-type"
  | "cseq-in-dialog-order"
  | "delay-2xx-on-unacked-reliable-1xx-with-sdp"
  | "delayed-offer-answered-in-ack"
  | "dialog-call-id-stable"
  | "direction-pair-valid"
  | "failed-reinvite-tears-down-dialog"
  | "final-2xx-answers-the-offer"
  | "from-tag-presence"
  | "in-dialog-from-tag"
  | "in-dialog-to-tag"
  | "max-forwards"
  | "mid-dialog-route"
  | "mid-dialog-tags"
  | "mid-dialog-uri"
  | "mid-dialog-wire-destination"
  | "no-100rel-require-on-non-invite"
  | "no-1xx-after-final"
  | "no-200-after-cancel"
  | "no-ack-to-dialog-creating-2xx"
  | "no-bye-outside-or-early-dialog"
  | "no-cancel-after-final"
  | "no-contact-on-bye"
  | "no-new-offer-while-offer-pending"
  | "no-new-reliable-1xx-after-final"
  | "no-overlapping-reliable-provisionals"
  | "no-prack-of-100-trying"
  | "no-prack-of-out-of-order-rseq"
  | "no-re-invite-while-invite-in-progress"
  | "no-record-route-from-ua"
  | "no-reliable-1xx-on-in-dialog"
  | "no-require-on-cancel-or-ack"
  | "no-target-404"
  | "no-to-tag-on-initial-request"
  | "non-contiguous-rseq"
  | "options-response-echoes"
  | "payload-type-mapping-stable"
  | "peer-uri-stable"
  | "prack-2xx-or-481"
  | "prack-answers-1xx-offer"
  | "proxy-100-trying-not-forwarded"
  | "proxy-100-within-grace"
  | "rack-without-known-invite"
  | "re-offer-m-line-count-monotonic"
  | "record-route-placement"
  | "register-no-route-set"
  | "rejected-stream-minimal-answer"
  | "reliable-1xx-headers"
  | "reliable-needs-client-opt-in"
  | "require-reliable-1xx-on-require"
  | "response-correlation"
  | "response-cseq-matches-transaction"
  | "response-echoes-request-via"
  | "rport-echo"
  | "rung-byte-identical"
  | "sdp-body-parseable"
  | "sdp-origin-continuity"
  | "second-answer-repeats-the-first"
  | "serial-register"
  | "single-final-per-server-txn"
  | "strict-route-rewrite-handled"
  | "strict-route-shuffle-on-send"
  | "tag-consistency"
  | "taken-final-ends-invite-txn"
  | "to-tag-presence"
  | "unacked-2xx-not-cleared"
  | "unacked-invite-non-2xx-final"
  | "unacked-reliable-provisional"
  | "unknown-dialog-481"
  | "unmatched-prack-proxied"
  | "unsupported-415-accepts"
  | "unsupported-extension-420"
  | "unsupported-extension-421"
  | "unsupported-method-405-allow"
  | "zero-port-propagation"
/**
 * What an endpoint IS relative to the system under test.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Side".
 */
export type Side = "peer" | "sut"
/**
 * The `op` of an `unordered` node.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "UnorderedOp".
 */
export type UnorderedOp = "unordered"
/**
 * Which side of a fix a document describes.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Variant".
 */
export type Variant = "repro" | "target"

/**
 * One replayable case: a captured call reproduced, or an authored test.
 */
export interface PivotV3 {
  /**
   * Simulated elements on those sockets.
   */
  actors: Actor[]
  /**
   * The calls the document plays. A captured case is one call; a
   * concurrency test is several whose flows interleave.
   */
  calls: Call[]
  case: Case
  /**
   * Peer non-compliance the replay must reproduce.
   */
  deviations?: Deviation[]
  /**
   * Sockets the lane must bind.
   */
  endpoints: Endpoint[]
  /**
   * The choreography: message steps, injected events and declared
   * alternatives, in order.
   */
  flow: FlowNode[]
  /**
   * Every number and domain the document names, declared once. Attempts,
   * actors and `${num:…}` accessors reference an entry by name; the driver
   * binds each name to a real number per lane.
   */
  identities: Identity[]
  /**
   * Symbolic dialogs.
   */
  legs: Leg[]
  /**
   * RESERVED (§12): the deployment-extensible media vocabulary. Nothing
   * reads it yet; it exists so the vocabulary lands additively rather than
   * as a version bump.
   */
  media?: {
    [k: string]: unknown
  } | null
  /**
   * The failures this run MUST produce. A document that declares any is a
   * NEGATIVE case: it replays a source whose non-compliance this platform
   * does not share, so the run cannot pass by behaving well, and the
   * document says in advance exactly how it fails.
   */
  must_fail?: MustFail[]
  /**
   * Always `3`. Exported as a `const`, so a mirror validating
   * against the schema refuses an earlier version rather than accepting a
   * document this crate would then reject.
   */
  pivot_version: 3
  /**
   * What must hold once the flow has run and everything has settled.
   */
  postconditions?: Postconditions | null
  /**
   * RFC rules a message this flow already carries breaks. Behavioural, so
   * the replay reproduces one by running the flow unchanged; `deviations`
   * covers the ones that change an emission.
   */
  rfc_violations?: RfcViolation[]
  timing: Timing
}
/**
 * One simulated network element.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Actor".
 */
export interface Actor {
  /**
   * Traffic this actor answers OUTSIDE the flow: matching messages never
   * move the flow cursor.
   */
  background?: BackgroundPolicy[]
  /**
   * How a UAS claims its inbound INVITE.
   */
  claim?: Claim | null
  /**
   * Endpoint id.
   */
  endpoint: string
  /**
   * Unique within the document; what a leg names.
   */
  id: string
  /**
   * Name of the actor's own identity in the `identities` registry — the
   * CALLER's, in practice: a callee's identity is named by its attempt.
   */
  identity?: string | null
  /**
   * The SIP role the actor plays.
   */
  kind: "uac" | "uas" | "mrf"
}
/**
 * One class of background traffic an actor answers, and what the run must
 * have seen of it by settle.
 *
 * A matching message is answered per `respond` and recorded; it never
 * satisfies an `expect`, and no flow step is ever written for one. An
 * assertion about background traffic is a COUNTER checked at settle, because
 * "the endpoint was polled while the call ran" is a fact about a period, not
 * about a position in a sequence.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BackgroundPolicy".
 */
export interface BackgroundPolicy {
  /**
   * How many such messages the run must have seen, checked at settle.
   */
  count?: CountBound | null
  match: BackgroundMatch
  respond: BackgroundResponse
}
/**
 * A settle-time count assertion. Every bound stated is checked; stating none
 * means the traffic is answered and not asserted about.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CountBound".
 */
export interface CountBound {
  /**
   * Lower bound on the messages this policy answered.
   */
  at_least?: number | null
  /**
   * Upper bound. `at_most: 0` and `exactly: 0` are how a document states an
   * absence.
   */
  at_most?: number | null
  /**
   * Exact count. Exclusive with the two bounds above.
   */
  exactly?: number | null
}
/**
 * What the policy answers.
 */
export interface BackgroundMatch {
  /**
   * The method this policy answers. Matching messages never move the flow
   * cursor and never satisfy an `expect`.
   */
  method: string
}
/**
 * How it answers.
 */
export interface BackgroundResponse {
  /**
   * The status the actor answers with. Mandatory: a policy is never silent.
   */
  status: number
}
/**
 * How a UAS decides that an inbound INVITE is the one it is playing. Carries
 * `by` and nothing else: which attempt an actor plays is already stated by
 * `calls[].attempts[].leg`, and restating it would give the chain two
 * encodings.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Claim".
 */
export interface Claim {
  /**
   * What tells this actor's inbound INVITE from another's.
   */
  by: "ruri-pos" | "arrival-order"
}
/**
 * One call: who places it, where it is routed, and how the system handled
 * provisionals for it.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Call".
 */
export interface Call {
  /**
   * The CALLER abandoned the call before any called leg crossed this
   * vantage. Exclusive with a chain and with `refused`: the platform took no
   * routing decision here, so nothing it sent the caller states one.
   */
  abandoned?: Abandoned | null
  /**
   * The attempt chain, in one encoding. Empty exactly when `refused` or
   * `abandoned` says why the call dialed nobody.
   */
  attempts: Attempt[]
  /**
   * The leg that originates the call.
   */
  caller_leg: string
  /**
   * Unique within the document. Tier-2 position tokens qualify with it
   * (`<call-id>.called[b][s]`) once a document declares more than one call.
   */
  id: string
  /**
   * The routing decision REFUSED the call: the platform dialed nobody and
   * answered the caller itself. Exclusive with a chain — a call either has
   * attempts or has none because it was refused.
   */
  refused?: Refused | null
  /**
   * The provisional-handling profile the captured system ran for THIS call.
   */
  relay18x?: Relay18X | null
}
/**
 * A call the CALLER abandoned before any dial crossed this vantage.
 *
 * The CANCEL itself is NOT restated here: `step` names the flow step carrying
 * it, so what the caller sent and when it sent it has one home. The final that
 * follows a cancelled INVITE is the `487` RFC 3261 §9.2 owes it and states no
 * routing decision, which is why an abandon is not a [`Refused`].
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Abandoned".
 */
export interface Abandoned {
  /**
   * Why the cut reads this vantage as an abandon rather than as a decision
   * the platform took.
   */
  evidence?: string | null
  /**
   * The caller-leg step carrying the CANCEL the caller SENT.
   */
  step: string
}
/**
 * One dialed attempt in a call's chain.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Attempt".
 */
export interface Attempt {
  /**
   * Parallel branch index.
   */
  branch: number
  callee: Callee
  /**
   * Why the platform left this attempt.
   */
  cause?: Cause | null
  /**
   * The captured signals that justify `cause`. Belongs to the attempt that
   * FAILED.
   */
  cause_evidence?: string[]
  /**
   * The attempt's own terminal INVITE final at its vantage. What makes the
   * chain barrier derivable without a per-step ordering field.
   */
  final?: Final | null
  /**
   * Why the correlator put THIS attempt in that chain. Belongs to the
   * attempt that JOINED.
   */
  join_evidence?: string | null
  /**
   * What ADDED this leg to the call: a transfer, or a media resource
   * inserted into it. A joined leg is a parallel arrival, not a failure —
   * the first attempt answered and the call then dialed somebody else — so
   * it is exclusive with the failure-only sequential `cause`.
   */
  joined_by?: JoinedBy | null
  /**
   * The leg id the flow uses for this attempt.
   */
  leg: string
  /**
   * First-18x-to-CANCEL dwell on the attempt's leg, ms, present exactly
   * when `cause` is `no-answer`.
   */
  no_answer_ms?: number | null
  /**
   * Position within the branch's sequential chain. Explicit, not array
   * order, so `(branch, position)` stays a stable key under sorted-key
   * formatting.
   */
  position: number
}
/**
 * Who the attempt dials.
 */
export interface Callee {
  /**
   * Name of an entry in the document's `identities` registry. The number
   * itself is never embedded here: which number a name becomes is a per-lane
   * binding the driver performs.
   */
  identity: string
}
/**
 * An attempt's terminal INVITE final at its own vantage.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Final".
 */
export interface Final {
  /**
   * When it arrived, in milliseconds from the case's first message.
   */
  at_ms: number
  /**
   * The final response's status code.
   */
  status: number
}
/**
 * What added a leg to a call that was already running.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "JoinedBy".
 */
export interface JoinedBy {
  /**
   * Which mechanism joined the leg.
   */
  kind: "refer" | "info" | "mrf"
  /**
   * The flow step that performed the join — the REFER or INFO the platform
   * accepted, or the request that inserted the media resource. It must be a step of a
   * leg of the SAME call: a join is an event inside one call, and naming
   * another call's step would make the chain unreadable.
   */
  step: string
}
/**
 * A call the routing decision refused before any dial.
 *
 * The final itself is NOT restated here: `step` names the flow step carrying
 * it, so the status, the reason phrase and the headers the caller got have one
 * home and cannot disagree with the flow.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Refused".
 */
export interface Refused {
  /**
   * Why the cut reads this vantage as a refusal rather than as a dial it
   * lost.
   */
  evidence?: string | null
  /**
   * The caller-leg step carrying the failure final the platform answered
   * the call with.
   */
  step: string
}
/**
 * The provisional-handling profile the CAPTURED system ran, in the routing
 * API's own vocabulary so a lane applies it without re-deciding. Every token
 * is open: these name a platform's configuration, not a SIP constant.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Relay18x".
 */
export interface Relay18X {
  /**
   * The detection signals that fired. Rides beside the fact, so a reviewer
   * reads the evidence rather than the verdict alone.
   */
  evidence: string[]
  /**
   * Open token: how many upstream `18x` reach the caller.
   */
  messages: string
  /**
   * Open profile token (how upstream `18x` are rewritten downstream).
   */
  mode: string
  /**
   * Open prack-mode token, present only where the captured system answered
   * 100rel itself.
   */
  prack?: string | null
}
/**
 * Identity, provenance, replayability, informative annotations.
 */
export interface Case {
  /**
   * Informative sidecar. The interpreter never reads it.
   */
  annotations?: Annotations | null
  /**
   * The step whose outcome IS the defect, plus a description.
   */
  defect?: Defect | null
  /**
   * Informative classification of the callflow shape. Open token; never
   * interpreted by an interpreter or a lane.
   */
  family: string
  /**
   * Case directory name.
   */
  id: string
  /**
   * Per-lane replayability verdict, keyed by lane name. The generator STATES
   * whether a lane can replay the case; a driver skips a non-replayable lane
   * loudly instead of failing it.
   */
  lanes: {
    [k: string]: LaneVerdict
  }
  /**
   * Where the document came from. What the generator-subset lint gate turns
   * on: a captured document may carry only what a capture can justify.
   */
  origin: "capture" | "authored"
  /**
   * The lane whose system produced the content this document asserts — the
   * captured platform, or the deployment a test was authored against. Open
   * token, and the one input to the lane-scoped downgrade of §9.1: replayed
   * on another lane, a CLASSIFIED check is evaluated and recorded without
   * gating. A document that states none downgrades nothing.
   */
  origin_lane?: string | null
  /**
   * Capabilities the rig must have for this case to mean anything (`proxy`,
   * `ha-pair`, `store-faults`, …). INFORMATIVE and open: the driver refuses
   * an impossible scenario/scene pairing loudly instead of running a test
   * that cannot fail. The interpreter never reads it.
   */
  requires?: string[]
  /**
   * Where the case was cut from. Present exactly on a captured document.
   */
  source?: Source | null
  /**
   * One line, human-facing.
   */
  title: string
  /**
   * Which side of a fix the document describes.
   */
  variant: "repro" | "target"
}
/**
 * Generator diagnostics and detection evidence that drives nothing, save the
 * `census-uncovered` flag, which a run's RFC audit reads as the document's
 * word that it states none of its scripted parties' violations (§11.1).
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Annotations".
 */
export interface Annotations {
  /**
   * Decisions the generator owes a human, one per diagnostic.
   */
  flags?: Flag[]
  /**
   * Free prose for a reviewer: what the case proves, and what it does not.
   */
  notes?: string[]
}
/**
 * One generator diagnostic: a decision owed, stated rather than guessed.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Flag".
 */
export interface Flag {
  /**
   * What the generator saw, and what it could not decide.
   */
  detail: string
  /**
   * Open token classifying the diagnostic, so a corpus is greppable by it.
   */
  kind: string
}
/**
 * The defect a repro case exists to hold still.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Defect".
 */
export interface Defect {
  /**
   * What is wrong, in one human-facing sentence.
   */
  description: string
  marker: DefectMarker
}
/**
 * Where in the flow the defect shows.
 */
export interface DefectMarker {
  /**
   * Id of the step whose outcome IS the defect.
   */
  step: string
}
/**
 * Where the case was cut from.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Source".
 */
export interface Source {
  /**
   * Whether identities went through the anonymizer.
   */
  anonymized: boolean
  /**
   * Upstream call groups the case spans.
   */
  call_groups: number[]
  /**
   * Capture file the case was cut from.
   */
  capture: string
}
/**
 * One reproduced non-compliance.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Deviation".
 */
export interface Deviation {
  /**
   * The header the deviation targets, canonically named. A deliberate
   * content-level malformation breaks ONE header's grammar in an otherwise
   * compliant message; without this the entry points at the step and an
   * interpreter cannot tell which of its own renderers to stand down.
   */
  header?: string | null
  /**
   * Unique within the document; what a review comment cites.
   */
  id: string
  /**
   * Open token naming the non-compliance. The kinds with a stated payload:
   * `cseq-override` (`value`), `suppress-auto` (`step`, an auto step),
   * `raw-order` and `verbatim-emission` (`preserve`).
   */
  kind: string
  /**
   * The leg the deviation applies to.
   */
  leg?: string | null
  /**
   * What must survive emission byte-for-byte (e.g. `header-order`,
   * `casing`).
   */
  preserve?: string[]
  /**
   * The other step this one races with.
   */
  races?: string | null
  /**
   * Retransmissions the capture shows, where the deviation IS the repeat.
   */
  retransmits?: number | null
  /**
   * The step id the deviation applies to. For `verbatim-emission` this is
   * the SCRIPTED send that triggers it: an auto step is the stack's own
   * composition, so asking this document to preserve its header order means
   * nothing. For `suppress-auto` it is precisely the auto step withheld —
   * which is why a withheld ACK stays a step with a deviation on it, rather
   * than a hole in the flow.
   */
  step?: string | null
  /**
   * The CSeq a `cseq-override` emits instead of the stack's own: an absolute
   * number, or an offset from a CSeq the run already saw.
   */
  value?: CseqValue | null
}
/**
 * Relative to a CSeq the run observed: `{ from: "${step:s7.cseq}",
 * delta: 1 }`. A capture cannot state this — the number it saw is
 * absolute — so it is an authored form.
 */
export interface Computed {
  /**
   * Signed offset applied to it.
   */
  delta: number
  /**
   * The accessor the value is read from.
   */
  from: string
}
/**
 * One socket the lane must bind.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Endpoint".
 */
export interface Endpoint {
  /**
   * How the lane must bind it.
   */
  binding: "dedicated" | "loopback"
  /**
   * Unique within the document; what an actor names.
   */
  id: string
  /**
   * The captured socket. A lane binding by address uses it; nothing else in
   * the document carries one.
   */
  observed: string
  /**
   * What the endpoint is relative to the system under test.
   */
  side: "peer" | "sut"
}
/**
 * One message at one actor's vantage.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Step".
 */
export interface Step {
  /**
   * Ids of steps that must have completed before this one runs. The one
   * cross-leg and cross-call ordering device.
   */
  after?: string[]
  /**
   * `true` when the interpreter's own stack COMPOSES this message (100
   * Trying, ACK-to-final, PRACK and its 2xx), deriving its R-URI, Route, Via
   * and CSeq from the transaction that obliged it rather than from dialog
   * state (§6.3). It is not a storage policy: the step stores what any step
   * stores, and only a body the stack could not place is withheld.
   */
  auto?: boolean
  /**
   * `expect` only: whether the stored content is matched or merely recorded.
   */
  check?: CheckMode | null
  /**
   * Field assertions over the matched message, in the one check vocabulary.
   */
  checks?: Check[]
  /**
   * `true` on the one ACK that answers a leg's dialog-creating final, and on
   * no other step (§6.1). It separates the ACK completing the dialog
   * handshake from an ordinary in-dialog ACK — a re-INVITE's — which the
   * method cannot and a CSeq chase may not. It rides BESIDE `in_dialog`,
   * never instead of it, and beside `early` where the ACK names its fork.
   */
  confirms_dialog?: boolean
  delay: Delay
  /**
   * Id of the early dialog the step rides: one a UAS-simulated fork answers
   * under (a response `send` carries the id), or one the run observes the
   * peer forking (a response `expect` carries it).
   */
  early?: string | null
  /**
   * Unique within the document. Tool-generated for a capture (`s7`),
   * author-chosen otherwise. Every reference in the document is to this id;
   * nothing references a position.
   */
  id: string
  /**
   * `true` when the message belongs to a transaction inside an ESTABLISHED
   * dialog. Generic, method-blind and TOTAL (§6.1): every step of a leg
   * after that leg's dialog-creating final carries it, none at or before it
   * does, and a CANCEL never does. What it marks on a final is that the final
   * answers a re-negotiation rather than creating the dialog, and an
   * in-dialog final never closes a leg (§4.1), so a `cause` may not cite one.
   */
  in_dialog?: boolean
  /**
   * The leg this message rides.
   */
  leg: string
  msg: MsgSpec
  /**
   * Capture coordinate. Captured documents only.
   */
  observed?: Observed | null
  /**
   * Whether the actor emits the message or waits for it.
   */
  op: "send" | "expect"
  /**
   * `expect` only: tolerated absence. The step is released when a later step
   * on the same leg matches first.
   */
  optional?: boolean
  /**
   * A declared race: this step and the named one may arrive in either order.
   */
  overlap?: string | null
  /**
   * The gap, in ms, before each repeat this step declares — one entry per
   * repeat, measured from the emission before it. A CAPTURED ladder's own
   * pacing: `retransmits` says how many rungs and this says when, so a peer
   * whose ladder is not the RFC's is replayed as it ran. Empty — on every
   * authored step, and on a generated one whose producer measured none — the
   * ladder falls back to the RFC pacing for the message class (§6.9).
   */
  retransmit_intervals_ms?: number[]
  /**
   * Captured retransmissions of THIS message, beyond the first. Appears on
   * `send` and `expect` alike: either side of a vantage can retransmit.
   */
  retransmits?: number | null
  /**
   * Per-step override of `timing.expect_budget_ms`.
   */
  within_ms?: number | null
}
/**
 * One field assertion.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Check".
 */
export interface Check {
  /**
   * Which vocabulary the check reads (§9.1). Stated where the assertion is
   * the ORIGIN platform's rather than SIP's, so a run on another lane
   * evaluates and records it without gating on it. An unclassified check
   * gates on every lane.
   */
  class?: CheckClass | null
  /**
   * Field selector. Open token: `from.userInfo`, `header(P-Asserted-Identity)`,
   * `body`, or a deployment observable's name in a postcondition.
   */
  field: string
  /**
   * How the field is compared.
   */
  op: "eq" | "regex" | "exists" | "absent"
  /**
   * Expected value: a literal, a regex (op `Regex`), or a string carrying
   * `${…}` accessors. Absent exactly for `Exists` and `Absent`.
   */
  value?: string | null
}
/**
 * When the step runs, relative to what.
 */
export interface Delay {
  /**
   * Whether a virtual-clock lane may compress this dwell. STATED, never
   * re-derived: a lane that compresses reads this and nothing else.
   */
  compressible: boolean
  /**
   * What the dwell is measured from: the case's start, or a named step.
   */
  from: string
  /**
   * Milliseconds from the anchor.
   */
  ms: number
  /**
   * Whether the dwell interacts with a system or session timer.
   */
  timer_linked: boolean
}
/**
 * The message itself, under the three-tier model (§8).
 */
export interface MsgSpec {
  /**
   * The body, by resource reference, multipart structure or asserted shape.
   */
  body?: Body | null
  /**
   * The captured CSeq NUMBER, on `auto` steps only: the LABEL of the
   * transaction that obliged the message. An identity token for pairing,
   * lint and confrontation, and the marker that scopes an auto ACK's DRAWN
   * `retransmits` to one transaction — NEVER replayed and never resolved
   * against: the interpreter's own stack numbers its CSeqs, and an auto ACK
   * finds the final it acknowledges in leg state.
   */
  cseq?: number | null
  /**
   * On a response: the CSeq method of the transaction it answers. A leg
   * carries the INVITE alongside every automatic, so without it an
   * automatic's 2xx satisfies the expectation meant for the call's answer.
   */
  "cseq-method"?: string | null
  /**
   * Tier-2 ref for the From address.
   */
  from?: Ref | null
  /**
   * Tier-3 frozen headers, in wire order with casing and duplicates
   * preserved. Stated on an `expect` regardless of `check`: under `assert`
   * they are matched, under `record` they are the recorded value.
   */
  headers?: Header[]
  /**
   * Existence checks on an expect.
   */
  "headers-present"?: string[]
  /**
   * Request discriminator.
   */
  method?: string | null
  /**
   * The response's reason phrase, as the capture carried it.
   */
  reason?: string | null
  /**
   * Tier-2 refs, on dialog-opening INVITE sends. An in-dialog R-URI is the
   * tier-1 learned remote target and is regenerated instead.
   */
  ruri?: Ref | null
  /**
   * Response discriminator.
   */
  status?: number | null
  /**
   * On a send: the leg's party refreshes its remote target here, for the
   * `n`-th time (RFC 3261 §12.1.2, §12.2.1.2). The Contact host:port stays
   * tier 1; the stack writes a user part of its own per refresh, kept on
   * every later message of the leg, so two messages the capture tells apart
   * by their Contact alone stay two messages on the wire.
   */
  "target-refresh"?: number | null
  /**
   * Tier-2 ref for the To address.
   */
  to?: Ref | null
}
/**
 * A decomposed multipart body: the parts are sibling resource files.
 */
export interface MultipartBody {
  multipart: Multipart
}
/**
 * The container and its parts.
 */
export interface Multipart {
  /**
   * The container's own media type as the wire wrote it, MINUS its
   * `boundary` parameter: the boundary — and only the boundary — is
   * regenerated at render, so every other parameter rides through.
   */
  "content-type": string
  /**
   * The parts, in wire order.
   */
  parts: Part[]
}
/**
 * One MIME part with its per-content-type handling.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Part".
 */
export interface Part {
  /**
   * Header names (lowercased) that reference this part by `cid:`. COMPUTED
   * from the part's `Content-ID` and the message's header list — it needs no
   * MIME parser, only the part id and the names.
   */
  "cid-linked"?: string[]
  /**
   * How an EXPECT's received part is held against the resource; absent
   * means `exact`. Ignored on a send, where the part is emitted and
   * compared with nothing.
   */
  compare?: BodyCompare | null
  /**
   * The part's OWN `Content-ID` value, angle brackets as the wire wrote them
   * (RFC 2045 §7). It is what a `cid:` reference resolves against (RFC 5621
   * §3), so a part any header points at replays under the same id.
   */
  "content-id"?: string | null
  /**
   * The part's `Content-Type` value, VERBATIM and with its parameters —
   * the composer writes this line back unchanged. The body registry (§8.2)
   * matches on the bare type, so a parameter never changes the handling.
   */
  "content-type": string
  /**
   * The part's remaining entity headers, verbatim and in wire order —
   * `Content-Transfer-Encoding`, `Content-Disposition`, whatever else the
   * part states. `Content-Type` and `Content-ID` are held in their own
   * fields and are never repeated here.
   */
  headers?: EntityHeader[]
  /**
   * Handling mode, on a part the registry freezes.
   */
  mode?: BodyMode | null
  /**
   * Path of the part's resource file, relative to the case directory.
   */
  ref: string
  /**
   * Rewrite tokens the lane applies to this part before emission.
   */
  rewrite?: string[]
}
/**
 * One entity header of a MIME part, exactly as the part carried it (RFC 2045
 * §3). A part header states MIME, never the origin platform's vocabulary, so
 * unlike a message header (`crate::msg::Header`) it carries no check class.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "EntityHeader".
 */
export interface EntityHeader {
  /**
   * Header name, in the part's own spelling and casing.
   */
  name: string
  /**
   * Header value, verbatim.
   */
  value: string
}
/**
 * A single body stored as a resource file.
 */
export interface ResourceBody {
  /**
   * How an EXPECT's received body is held against the resource; absent
   * means `exact`. Meaningless on a send, where lint refuses it
   * (`body/compare-on-send`).
   */
  compare?: BodyCompare | null
  /**
   * The captured `Content-Type` value, VERBATIM and with its parameters, so
   * the body replays under the type the wire wrote. Omitted only where
   * render derives exactly it — a rewritten body whose type is bare
   * `application/sdp` — so the stored and derived values cannot drift.
   */
  "content-type"?: string | null
  /**
   * Handling mode, on a body the registry freezes.
   */
  mode?: BodyMode | null
  /**
   * Path of the resource file, relative to the case directory.
   */
  ref: string
  /**
   * Rewrite tokens the lane applies before emission (e.g. `c=addr`,
   * `m=port` on SDP). Omitted where the body replays byte-exact. On an
   * expect the tokens name the lane-owned fields the `sdp` compare masks
   * where the run rebooked media: `c=addr` the address of a `c=IN IP4`
   * line, `m=port` the non-zero port of an `m=` line, its `/count` kept.
   */
  rewrite?: string[]
}
/**
 * A declared shape, checked rather than emitted.
 */
export interface ShapeBody {
  /**
   * Which shape the expectation asserts.
   */
  mode: "sdp-present" | "absent" | "multipart-present"
}
/**
 * Role-mapped: the lane substitutes the number it allocated for this
 * position.
 */
export interface PositionalRef {
  /**
   * On a From or To: the captured name-addr, tag dropped, its role numbers
   * composed as `${num:…}`. Present, it is the field's whole value and the
   * lane composes no URI around it.
   */
  addr?: string | null
  /**
   * Open dial-form token: which form of the number this field carried.
   */
  form?: string | null
  /**
   * Position token in the tier-2 reference namespace, resolved against the
   * call chains: `caller` or `called[<branch>][<position>]`, call-qualified
   * (`c2.called[0][1]`) once the document declares more than one call.
   */
  pos: string
}
/**
 * Frozen: the plan did not recognize the value, so it replays verbatim.
 */
export interface FrozenRef {
  /**
   * On a From or To: the captured name-addr, tag dropped, its role numbers
   * composed as `${num:…}`. Present, it is the field's whole value and the
   * lane composes no URI around it.
   */
  addr?: string | null
  /**
   * The captured value, replayed verbatim.
   */
  frozen: string
  /**
   * Open plan token qualifying the frozen value where the plan classified
   * the ADDRESS without resolving a number (an anonymous caller carries no
   * number to map, but the class still drives emission).
   */
  kind?: string | null
}
/**
 * One frozen header, exactly as the capture carried it.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Header".
 */
export interface Header {
  /**
   * Which vocabulary this header is (§9.1), where it is the ORIGIN
   * platform's rather than SIP's. Under `check: assert` a classified header
   * is matched on the lane the document came from and merely recorded
   * elsewhere; an unclassified header is matched on every lane.
   */
  class?: CheckClass | null
  /**
   * Header name, in the capture's own spelling and casing.
   */
  name: string
  /**
   * Header value, verbatim — or, in an authored document, composed from
   * `${…}` accessors.
   */
  value: string
}
/**
 * The step's coordinate in the flows document. Informative: the interpreter
 * never reads it. It exists because the post-run confrontation has to pair a
 * run step with the captured message it is compared against.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Observed".
 */
export interface Observed {
  /**
   * Microseconds from the case's FIRST captured message.
   */
  at_us: number
  /**
   * Leg index in the flows document.
   */
  leg: number
  /**
   * Message index within that leg.
   */
  msg: number
}
/**
 * An external event the lane's injector performs. The interpreter NEVER
 * executes an action: it hands the token to an injector interface the
 * deployment provides, which is why one op spans store faults, HTTP-fabric
 * faults and node kills.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Inject".
 */
export interface Inject {
  /**
   * Open action token from the deployment's injector registry
   * (`store-fault:LiveAudit`, `http:decision-cut`, `node-kill`).
   */
  action: string
  /**
   * Ids of steps that must have completed before the action is handed over.
   */
  after?: string[]
  /**
   * Dwell before the action, anchored like a step's.
   */
  delay?: Delay1 | null
  /**
   * Unique within the document; what `after` and a delay anchor name.
   */
  id: string
  /**
   * Always `inject`.
   */
  op: "inject"
  /**
   * What the action applies to, in the injector's own vocabulary.
   */
  target?: string | null
}
/**
 * Dwell from an explicit anchor. There are no absolute times in a pivot.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Delay".
 */
export interface Delay1 {
  /**
   * Whether a virtual-clock lane may compress this dwell. STATED, never
   * re-derived: a lane that compresses reads this and nothing else.
   */
  compressible: boolean
  /**
   * What the dwell is measured from: the case's start, or a named step.
   */
  from: string
  /**
   * Milliseconds from the anchor.
   */
  ms: number
  /**
   * Whether the dwell interacts with a system or session timer.
   */
  timer_linked: boolean
}
/**
 * Declared alternatives. The interpreter commits on the first discriminating
 * message and NEVER backtracks, so branches must differ at their first
 * message — which lint enforces structurally rather than trusting an author.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Alt".
 */
export interface Alt {
  /**
   * Ids of steps that must have completed before the alt opens.
   */
  after?: string[]
  /**
   * The alternatives, two or more, discriminable at their first message.
   */
  branches: Branch[]
  /**
   * Unique within the document. Referenced from outside the alt in place of
   * any step inside it, and by `${step:<alt-id>.branch}`.
   */
  id: string
  /**
   * Always `alt`.
   */
  op: "alt"
}
/**
 * One alternative. Its `name` is what a later assertion cites
 * (`${step:<alt-id>.branch}`).
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Branch".
 */
export interface Branch {
  /**
   * Unique within the alt; what `${step:<alt-id>.branch}` resolves to.
   */
  name: string
  /**
   * The branch's messages, in order. Message steps only — a nested block
   * would have no first message to commit on.
   */
  steps: Step[]
}
/**
 * Messages that must ALL arrive, in any order — the declarative form of a
 * race whose outcome does not matter.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Unordered".
 */
export interface Unordered {
  /**
   * Ids of steps that must have completed before the group opens.
   */
  after?: string[]
  /**
   * Unique within the document; what a later reference names.
   */
  id: string
  /**
   * Always `unordered`.
   */
  op: "unordered"
  /**
   * The messages, all of which must arrive. They share one place in the
   * ordering, so none may reference another.
   */
  steps: Step[]
}
/**
 * One party's identity: what the plan made of it, under the name the rest of
 * the document refers to it by.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Identity".
 */
export interface Identity {
  /**
   * What the number catalog says about the value, when a catalog answered.
   */
  catalog?: CatalogEntry | null
  /**
   * Open dial-form tokens the plan resolved this value into, and the forms
   * `${num:<name>:<form>}` may ask for. Omitted when the plan resolved none.
   */
  forms?: string[]
  /**
   * Open plan token classifying the identity (e.g. a caller class, a
   * catalog class, or `unknown` when the plan does not recognize it).
   */
  kind: string
  /**
   * Unique within the document, and the only handle onto this identity:
   * `attempts[].callee`, `actors[].identity` and `${num:<name>:<form>}` all
   * name it. A generated document synthesizes the name from the party's
   * position (`caller`, `called-0-1`), so one encoding covers captured and
   * authored documents alike.
   */
  name: string
  /**
   * On a captured document: the anonymized value the capture carried, and
   * the provenance that discipline is about. On an authored one: optional
   * and purely informative — the lane binds the NAME, never this — so
   * prefer omitting it and letting `${num:…}` compose the value.
   */
  observed?: string | null
}
/**
 * The catalog's own classification of a provisioned number.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CatalogEntry".
 */
export interface CatalogEntry {
  /**
   * Open catalog class token. Deployment vocabulary — never interpreted
   * here.
   */
  class: string
}
/**
 * One symbolic dialog.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Leg".
 */
export interface Leg {
  /**
   * Actor id.
   */
  actor: string
  /**
   * Which side of the dialog the actor is on.
   */
  dir: "out" | "in"
  /**
   * Unique within the document; what every flow step and leg accessor names.
   */
  id: string
  /**
   * Per-leg RTP source. Omitted where the leg carries no media.
   */
  media?: Media | null
}
/**
 * Media the leg carries.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Media".
 */
export interface Media {
  /**
   * Open token naming how the lane sources RTP for this leg.
   */
  rtp: string
}
/**
 * One failure this run must produce, at or immediately after one step.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "MustFail".
 */
export interface MustFail {
  /**
   * The rule the SOURCE broke, whose violation predicts this failure. A
   * declaration is DERIVED from a detector's hit, never hand-guessed.
   */
  derived_from:
    | "ack-body-after-complete-offer-answer"
    | "ack-cseq-matches-invite"
    | "ack-preserves-invite-route"
    | "ack-require-subset-of-invite"
    | "allow-supported-on-invite"
    | "answer-m-line-count-matches-offer"
    | "answer-media-type-matches-offer"
    | "answer-stream-matches-offer"
    | "answer-t-line-equals-offer"
    | "branch-prefix"
    | "c0-port-non-zero"
    | "cancel-after-1xx"
    | "cancel-cseq-method"
    | "cancel-request-uri"
    | "cancel-route-echoes-invite"
    | "cancel-via-branch"
    | "concurrent-re-invite-500-or-491"
    | "contact-presence"
    | "content-length"
    | "content-type"
    | "cseq-in-dialog-order"
    | "delay-2xx-on-unacked-reliable-1xx-with-sdp"
    | "delayed-offer-answered-in-ack"
    | "dialog-call-id-stable"
    | "direction-pair-valid"
    | "failed-reinvite-tears-down-dialog"
    | "final-2xx-answers-the-offer"
    | "from-tag-presence"
    | "in-dialog-from-tag"
    | "in-dialog-to-tag"
    | "max-forwards"
    | "mid-dialog-route"
    | "mid-dialog-tags"
    | "mid-dialog-uri"
    | "mid-dialog-wire-destination"
    | "no-100rel-require-on-non-invite"
    | "no-1xx-after-final"
    | "no-200-after-cancel"
    | "no-ack-to-dialog-creating-2xx"
    | "no-bye-outside-or-early-dialog"
    | "no-cancel-after-final"
    | "no-contact-on-bye"
    | "no-new-offer-while-offer-pending"
    | "no-new-reliable-1xx-after-final"
    | "no-overlapping-reliable-provisionals"
    | "no-prack-of-100-trying"
    | "no-prack-of-out-of-order-rseq"
    | "no-re-invite-while-invite-in-progress"
    | "no-record-route-from-ua"
    | "no-reliable-1xx-on-in-dialog"
    | "no-require-on-cancel-or-ack"
    | "no-target-404"
    | "no-to-tag-on-initial-request"
    | "non-contiguous-rseq"
    | "options-response-echoes"
    | "payload-type-mapping-stable"
    | "peer-uri-stable"
    | "prack-2xx-or-481"
    | "prack-answers-1xx-offer"
    | "proxy-100-trying-not-forwarded"
    | "proxy-100-within-grace"
    | "rack-without-known-invite"
    | "re-offer-m-line-count-monotonic"
    | "record-route-placement"
    | "register-no-route-set"
    | "rejected-stream-minimal-answer"
    | "reliable-1xx-headers"
    | "reliable-needs-client-opt-in"
    | "require-reliable-1xx-on-require"
    | "response-correlation"
    | "response-cseq-matches-transaction"
    | "response-echoes-request-via"
    | "rport-echo"
    | "rung-byte-identical"
    | "sdp-body-parseable"
    | "sdp-origin-continuity"
    | "second-answer-repeats-the-first"
    | "serial-register"
    | "single-final-per-server-txn"
    | "strict-route-rewrite-handled"
    | "strict-route-shuffle-on-send"
    | "tag-consistency"
    | "taken-final-ends-invite-txn"
    | "to-tag-presence"
    | "unacked-2xx-not-cleared"
    | "unacked-invite-non-2xx-final"
    | "unacked-reliable-provisional"
    | "unknown-dialog-481"
    | "unmatched-prack-proxied"
    | "unsupported-415-accepts"
    | "unsupported-extension-420"
    | "unsupported-extension-421"
    | "unsupported-method-405-allow"
    | "zero-port-propagation"
  /**
   * What the run must produce.
   */
  failure: "unexpected-ack" | "unexpected-prack" | "unexpected-cancel"
  /**
   * The flow step the failure happens at or immediately after — the anchor,
   * so a reader lands on the datagram the divergence turns on.
   */
  step: string
}
/**
 * Assertions evaluated once, after settle.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Postconditions".
 */
export interface Postconditions {
  /**
   * The call-detail-record expectation. Required (lint), because a call test
   * that never looks at what was billed is half a test.
   */
  cdr?: CdrExpectation | null
  /**
   * Deployment observables — metric names, store contents — checked at
   * settle and never mid-flow. Open field vocabulary.
   */
  checks?: Check[]
}
/**
 * The records the run must have written.
 */
export interface CdrCheck {
  /**
   * Field assertions over them, in the one check vocabulary.
   */
  checks?: Check[]
  /**
   * How many records the run must have written.
   */
  count: number
}
/**
 * No CDR assertion, with the reason stated. An open token, so a
 * deployment names its own gaps; what is closed is that a gap must be
 * NAMED.
 */
export interface CdrAbsent {
  /**
   * Open reason token, e.g. `capture-carries-no-cdr`.
   */
  absent: string
}
/**
 * One RFC rule a message of this flow breaks.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "RfcViolation".
 */
export interface RfcViolation {
  /**
   * Who emitted it: an `actors` id, or `sut`. A scripted peer's entry is
   * listed and gates nothing; it cancels the run's finding of the same rule
   * against that actor on the transaction `step` names. The system under
   * test's gates, unless it `relays` one.
   */
  emitter: string
  /**
   * On a `sut` entry only: the step of a scripted party's entry of the same
   * rule that this one carries on — the captured platform relayed onward a
   * violation that party committed first. Such an entry gates nothing; with
   * the party's entry it cancels the run's finding of the rule against the
   * system under test on the transaction `step` names.
   */
  relays?: string | null
  /**
   * Which rule is broken.
   */
  rule:
    | "ack-body-after-complete-offer-answer"
    | "ack-cseq-matches-invite"
    | "ack-preserves-invite-route"
    | "ack-require-subset-of-invite"
    | "allow-supported-on-invite"
    | "answer-m-line-count-matches-offer"
    | "answer-media-type-matches-offer"
    | "answer-stream-matches-offer"
    | "answer-t-line-equals-offer"
    | "branch-prefix"
    | "c0-port-non-zero"
    | "cancel-after-1xx"
    | "cancel-cseq-method"
    | "cancel-request-uri"
    | "cancel-route-echoes-invite"
    | "cancel-via-branch"
    | "concurrent-re-invite-500-or-491"
    | "contact-presence"
    | "content-length"
    | "content-type"
    | "cseq-in-dialog-order"
    | "delay-2xx-on-unacked-reliable-1xx-with-sdp"
    | "delayed-offer-answered-in-ack"
    | "dialog-call-id-stable"
    | "direction-pair-valid"
    | "failed-reinvite-tears-down-dialog"
    | "final-2xx-answers-the-offer"
    | "from-tag-presence"
    | "in-dialog-from-tag"
    | "in-dialog-to-tag"
    | "max-forwards"
    | "mid-dialog-route"
    | "mid-dialog-tags"
    | "mid-dialog-uri"
    | "mid-dialog-wire-destination"
    | "no-100rel-require-on-non-invite"
    | "no-1xx-after-final"
    | "no-200-after-cancel"
    | "no-ack-to-dialog-creating-2xx"
    | "no-bye-outside-or-early-dialog"
    | "no-cancel-after-final"
    | "no-contact-on-bye"
    | "no-new-offer-while-offer-pending"
    | "no-new-reliable-1xx-after-final"
    | "no-overlapping-reliable-provisionals"
    | "no-prack-of-100-trying"
    | "no-prack-of-out-of-order-rseq"
    | "no-re-invite-while-invite-in-progress"
    | "no-record-route-from-ua"
    | "no-reliable-1xx-on-in-dialog"
    | "no-require-on-cancel-or-ack"
    | "no-target-404"
    | "no-to-tag-on-initial-request"
    | "non-contiguous-rseq"
    | "options-response-echoes"
    | "payload-type-mapping-stable"
    | "peer-uri-stable"
    | "prack-2xx-or-481"
    | "prack-answers-1xx-offer"
    | "proxy-100-trying-not-forwarded"
    | "proxy-100-within-grace"
    | "rack-without-known-invite"
    | "re-offer-m-line-count-monotonic"
    | "record-route-placement"
    | "register-no-route-set"
    | "rejected-stream-minimal-answer"
    | "reliable-1xx-headers"
    | "reliable-needs-client-opt-in"
    | "require-reliable-1xx-on-require"
    | "response-correlation"
    | "response-cseq-matches-transaction"
    | "response-echoes-request-via"
    | "rport-echo"
    | "rung-byte-identical"
    | "sdp-body-parseable"
    | "sdp-origin-continuity"
    | "second-answer-repeats-the-first"
    | "serial-register"
    | "single-final-per-server-txn"
    | "strict-route-rewrite-handled"
    | "strict-route-shuffle-on-send"
    | "tag-consistency"
    | "taken-final-ends-invite-txn"
    | "to-tag-presence"
    | "unacked-2xx-not-cleared"
    | "unacked-invite-non-2xx-final"
    | "unacked-reliable-provisional"
    | "unknown-dialog-481"
    | "unmatched-prack-proxied"
    | "unsupported-415-accepts"
    | "unsupported-extension-420"
    | "unsupported-extension-421"
    | "unsupported-method-405-allow"
    | "zero-port-propagation"
  /**
   * The flow step whose message breaks it — the anchor, so a reader lands on
   * the datagram rather than on a paragraph.
   */
  step: string
}
/**
 * Budgets stated once.
 */
export interface Timing {
  /**
   * The CASE's span — the last flow step's `observed.at_us` in
   * milliseconds. Captured documents only: an authored test has no capture
   * to span.
   */
  capture_span_ms?: number | null
  /**
   * Default assertion timeout for every `expect`; a step's `within_ms`
   * overrides it per step.
   */
  expect_budget_ms: number
  /**
   * The ceiling on the settle phase (§10): how long the runner waits for
   * every scripted dialog to reach a terminal state and for the system's
   * own call count to fall to zero. Failing to settle inside it is test
   * failure — there is no soft mode.
   */
  settle_budget_ms: number
}
/**
 * What a background policy matches. Method-only today: a keepalive is
 * identified by its method, and a policy that had to inspect a header would
 * be a flow step wearing a disguise.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BackgroundMatch".
 */
export interface BackgroundMatch1 {
  /**
   * The method this policy answers. Matching messages never move the flow
   * cursor and never satisfy an `expect`.
   */
  method: string
}
/**
 * How a background policy answers.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "BackgroundResponse".
 */
export interface BackgroundResponse1 {
  /**
   * The status the actor answers with. Mandatory: a policy is never silent.
   */
  status: number
}
/**
 * The dialed target.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Callee".
 */
export interface Callee1 {
  /**
   * Name of an entry in the document's `identities` registry. The number
   * itself is never embedded here: which number a name becomes is a per-lane
   * binding the driver performs.
   */
  identity: string
}
/**
 * Case identity, provenance and replayability.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Case".
 */
export interface Case1 {
  /**
   * Informative sidecar. The interpreter never reads it.
   */
  annotations?: Annotations | null
  /**
   * The step whose outcome IS the defect, plus a description.
   */
  defect?: Defect | null
  /**
   * Informative classification of the callflow shape. Open token; never
   * interpreted by an interpreter or a lane.
   */
  family: string
  /**
   * Case directory name.
   */
  id: string
  /**
   * Per-lane replayability verdict, keyed by lane name. The generator STATES
   * whether a lane can replay the case; a driver skips a non-replayable lane
   * loudly instead of failing it.
   */
  lanes: {
    [k: string]: LaneVerdict
  }
  /**
   * Where the document came from. What the generator-subset lint gate turns
   * on: a captured document may carry only what a capture can justify.
   */
  origin: "capture" | "authored"
  /**
   * The lane whose system produced the content this document asserts — the
   * captured platform, or the deployment a test was authored against. Open
   * token, and the one input to the lane-scoped downgrade of §9.1: replayed
   * on another lane, a CLASSIFIED check is evaluated and recorded without
   * gating. A document that states none downgrades nothing.
   */
  origin_lane?: string | null
  /**
   * Capabilities the rig must have for this case to mean anything (`proxy`,
   * `ha-pair`, `store-faults`, …). INFORMATIVE and open: the driver refuses
   * an impossible scenario/scene pairing loudly instead of running a test
   * that cannot fail. The interpreter never reads it.
   */
  requires?: string[]
  /**
   * Where the case was cut from. Present exactly on a captured document.
   */
  source?: Source | null
  /**
   * One line, human-facing.
   */
  title: string
  /**
   * Which side of a fix the document describes.
   */
  variant: "repro" | "target"
}
/**
 * A stated absence of a CDR assertion.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CdrAbsent".
 */
export interface CdrAbsent1 {
  /**
   * Open reason token, e.g. `capture-carries-no-cdr`.
   */
  absent: string
}
/**
 * The CDR count the run must produce, plus any field assertions over them.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "CdrCheck".
 */
export interface CdrCheck1 {
  /**
   * Field assertions over them, in the one check vocabulary.
   */
  checks?: Check[]
  /**
   * How many records the run must have written.
   */
  count: number
}
/**
 * The one computable form: a referenced value plus a signed offset. There is
 * no expression language, deliberately — `{ from, delta }` covers "the CSeq
 * the peer sent, plus one", which is every arithmetic the corpus and the
 * ported tests need.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Computed".
 */
export interface Computed1 {
  /**
   * Signed offset applied to it.
   */
  delta: number
  /**
   * The accessor the value is read from.
   */
  from: string
}
/**
 * The flow step whose outcome IS the defect, named by its id — so inserting a
 * step ahead of it does not move the marker.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "DefectMarker".
 */
export interface DefectMarker1 {
  /**
   * Id of the step whose outcome IS the defect.
   */
  step: string
}
/**
 * A tier-2 reference the numbering plan did not resolve.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "FrozenRef".
 */
export interface FrozenRef1 {
  /**
   * On a From or To: the captured name-addr, tag dropped, its role numbers
   * composed as `${num:…}`. Present, it is the field's whole value and the
   * lane composes no URI around it.
   */
  addr?: string | null
  /**
   * The captured value, replayed verbatim.
   */
  frozen: string
  /**
   * Open plan token qualifying the frozen value where the plan classified
   * the ADDRESS without resolving a number (an anonymous caller carries no
   * number to map, but the class still drives emission).
   */
  kind?: string | null
}
/**
 * The message a step sends or expects.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "MsgSpec".
 */
export interface MsgSpec1 {
  /**
   * The body, by resource reference, multipart structure or asserted shape.
   */
  body?: Body | null
  /**
   * The captured CSeq NUMBER, on `auto` steps only: the LABEL of the
   * transaction that obliged the message. An identity token for pairing,
   * lint and confrontation, and the marker that scopes an auto ACK's DRAWN
   * `retransmits` to one transaction — NEVER replayed and never resolved
   * against: the interpreter's own stack numbers its CSeqs, and an auto ACK
   * finds the final it acknowledges in leg state.
   */
  cseq?: number | null
  /**
   * On a response: the CSeq method of the transaction it answers. A leg
   * carries the INVITE alongside every automatic, so without it an
   * automatic's 2xx satisfies the expectation meant for the call's answer.
   */
  "cseq-method"?: string | null
  /**
   * Tier-2 ref for the From address.
   */
  from?: Ref | null
  /**
   * Tier-3 frozen headers, in wire order with casing and duplicates
   * preserved. Stated on an `expect` regardless of `check`: under `assert`
   * they are matched, under `record` they are the recorded value.
   */
  headers?: Header[]
  /**
   * Existence checks on an expect.
   */
  "headers-present"?: string[]
  /**
   * Request discriminator.
   */
  method?: string | null
  /**
   * The response's reason phrase, as the capture carried it.
   */
  reason?: string | null
  /**
   * Tier-2 refs, on dialog-opening INVITE sends. An in-dialog R-URI is the
   * tier-1 learned remote target and is regenerated instead.
   */
  ruri?: Ref | null
  /**
   * Response discriminator.
   */
  status?: number | null
  /**
   * On a send: the leg's party refreshes its remote target here, for the
   * `n`-th time (RFC 3261 §12.1.2, §12.2.1.2). The Contact host:port stays
   * tier 1; the stack writes a user part of its own per refresh, kept on
   * every later message of the leg, so two messages the capture tells apart
   * by their Contact alone stay two messages on the wire.
   */
  "target-refresh"?: number | null
  /**
   * Tier-2 ref for the To address.
   */
  to?: Ref | null
}
/**
 * The multipart container and its parts.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Multipart".
 */
export interface Multipart1 {
  /**
   * The container's own media type as the wire wrote it, MINUS its
   * `boundary` parameter: the boundary — and only the boundary — is
   * regenerated at render, so every other parameter rides through.
   */
  "content-type": string
  /**
   * The parts, in wire order.
   */
  parts: Part[]
}
/**
 * A multipart body, referencing its already-decomposed parts.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "MultipartBody".
 */
export interface MultipartBody1 {
  multipart: Multipart
}
/**
 * A tier-2 reference the numbering plan resolved.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "PositionalRef".
 */
export interface PositionalRef1 {
  /**
   * On a From or To: the captured name-addr, tag dropped, its role numbers
   * composed as `${num:…}`. Present, it is the field's whole value and the
   * lane composes no URI around it.
   */
  addr?: string | null
  /**
   * Open dial-form token: which form of the number this field carried.
   */
  form?: string | null
  /**
   * Position token in the tier-2 reference namespace, resolved against the
   * call chains: `caller` or `called[<branch>][<position>]`, call-qualified
   * (`c2.called[0][1]`) once the document declares more than one call.
   */
  pos: string
}
/**
 * A single body carried by a resource file.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "ResourceBody".
 */
export interface ResourceBody1 {
  /**
   * How an EXPECT's received body is held against the resource; absent
   * means `exact`. Meaningless on a send, where lint refuses it
   * (`body/compare-on-send`).
   */
  compare?: BodyCompare | null
  /**
   * The captured `Content-Type` value, VERBATIM and with its parameters, so
   * the body replays under the type the wire wrote. Omitted only where
   * render derives exactly it — a rewritten body whose type is bare
   * `application/sdp` — so the stored and derived values cannot drift.
   */
  "content-type"?: string | null
  /**
   * Handling mode, on a body the registry freezes.
   */
  mode?: BodyMode | null
  /**
   * Path of the resource file, relative to the case directory.
   */
  ref: string
  /**
   * Rewrite tokens the lane applies before emission (e.g. `c=addr`,
   * `m=port` on SDP). Omitted where the body replays byte-exact. On an
   * expect the tokens name the lane-owned fields the `sdp` compare masks
   * where the run rebooked media: `c=addr` the address of a `c=IN IP4`
   * line, `m=port` the non-zero port of an `m=` line, its `/count` kept.
   */
  rewrite?: string[]
}
/**
 * A body asserted by SHAPE rather than content.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "ShapeBody".
 */
export interface ShapeBody1 {
  /**
   * Which shape the expectation asserts.
   */
  mode: "sdp-present" | "absent" | "multipart-present"
}
/**
 * Budgets stated once for the whole case.
 *
 * This interface was referenced by `PivotV3`'s JSON-Schema
 * via the `definition` "Timing".
 */
export interface Timing1 {
  /**
   * The CASE's span — the last flow step's `observed.at_us` in
   * milliseconds. Captured documents only: an authored test has no capture
   * to span.
   */
  capture_span_ms?: number | null
  /**
   * Default assertion timeout for every `expect`; a step's `within_ms`
   * overrides it per step.
   */
  expect_budget_ms: number
  /**
   * The ceiling on the settle phase (§10): how long the runner waits for
   * every scripted dialog to reach a terminal state and for the system's
   * own call count to fall to zero. Failing to settle inside it is test
   * failure — there is no soft mode.
   */
  settle_budget_ms: number
}

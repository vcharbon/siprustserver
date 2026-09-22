/**
 * The **run verdict** as `verdict.json`, mirroring
 * `pivot_schema::bundle::verdict`: what the run decided, and — where it failed —
 * enough to diagnose it without re-running.
 *
 * Every failure names its site. Nothing is downgraded to a warning to make a run
 * pass, and a run that did not fully settle is a FAILURE, always.
 *
 * `Failure` is internally tagged on `failure`, kebab-case, with snake_case
 * payload fields. There is deliberately no catch-all variant that maps distinct
 * failures onto one bland shape, so this union carries one member per site.
 */
import * as Schema from "effect/Schema"
import { CheckClass, KnownBug } from "../check.js"
import { DeclaredFailure } from "../must-fail.js"
import { nullable } from "../serde.js"
import { RfcRule } from "../violation.js"

/** The run's outcome. `ok-negative` is spelled apart so nothing reads a case that passed BY FAILING as a case that passed. */
export const VerdictStatus = Schema.Literals(["ok", "ok-negative", "failed"])
export type VerdictStatus = typeof VerdictStatus.Type

const failure = <const K extends string, F extends Schema.Struct.Fields>(kind: K, fields: F) =>
  Schema.Struct({ failure: Schema.Literal(kind), ...fields })

/**
 * The structural identity of one datagram a failure names. Fields, never prose:
 * the confrontation builds its substitution probes from these, so nothing here
 * re-parses a description.
 */
export const Arrived = Schema.Union([
  Schema.Struct({ kind: Schema.Literal("request"), method: Schema.String, cseq: Schema.Int }),
  Schema.Struct({
    kind: Schema.Literal("response"),
    status: Schema.Int,
    reason: Schema.String,
    cseq_method: Schema.String,
    cseq: Schema.Int
  }),
  Schema.Struct({ kind: Schema.Literal("unreadable") })
])
export type Arrived = typeof Arrived.Type

/** What a refusing expect gated on: its discriminator, as data. */
export const GatedOn = Schema.Union([
  Schema.Struct({ kind: Schema.Literal("request"), method: Schema.String }),
  Schema.Struct({
    kind: Schema.Literal("response"),
    status: Schema.Int,
    cseq_method: Schema.optionalKey(Schema.String)
  })
])
export type GatedOn = typeof GatedOn.Type

/** The document does not compile to a plan. */
export const PlanRefused = failure("plan-refused", { detail: Schema.String })
/** An `expect` was not satisfied inside its budget. */
export const ExpectTimedOut = failure("expect-timed-out", {
  step: Schema.String,
  leg: Schema.String,
  gated_on: Schema.String,
  within_ms: Schema.Int
})
/**
 * A datagram arrived on the leg that the armed expect does not match, and no
 * background policy answers it. `reason` is the closest gate's own words.
 */
export const UnmatchedDatagram = failure("unmatched-datagram", {
  step: Schema.String,
  leg: Schema.String,
  gated_on: GatedOn,
  reason: Schema.String,
  arrived: Arrived
})
/** A datagram arrived on a leg with no armed expect at all. */
export const UnexpectedDatagram = failure("unexpected-datagram", {
  leg: Schema.String,
  arrived: Arrived,
  detail: Schema.optionalKey(Schema.String)
})
/** A datagram arrived during the settle window of a flow that had COMPLETED. */
export const DatagramAfterFlow = failure("datagram-after-flow", {
  leg: Schema.String,
  arrived: Arrived
})
/** A non-2xx final a scripted leg sent to an INVITE drew no ACK inside Timer H (RFC 3261 §17.2.1). */
export const FinalUnacknowledged = failure("final-unacknowledged", {
  leg: Schema.String,
  status: Schema.Int,
  cseq: Schema.Int
})
/** An inline or postcondition check did not hold. */
export const CheckFailed = failure("check-failed", {
  site: Schema.String,
  field: Schema.String,
  op: Schema.String,
  expected: Schema.String,
  observed: Schema.String
})
/** A `${…}` could not be resolved at the moment it was read. */
export const AccessorUnresolved = failure("accessor-unresolved", {
  site: Schema.String,
  detail: Schema.String
})
/** An emission could not be rendered or sent. */
export const SendFailed = failure("send-failed", {
  step: Schema.String,
  leg: Schema.String,
  detail: Schema.String
})
/** The run did not reach a settled state inside `timing.settle_budget_ms`. */
export const SettleTimedOut = failure("settle-timed-out", {
  budget_ms: Schema.Int,
  open: Schema.Array(Schema.String)
})
/** A background policy's settle-time count bound did not hold. */
export const BackgroundCount = failure("background-count", {
  actor: Schema.String,
  method: Schema.String,
  bound: Schema.String,
  observed: Schema.Int
})
/** The CDR expectation did not hold. */
export const CdrMismatch = failure("cdr-mismatch", {
  expected: Schema.String,
  observed: Schema.String
})
/** The flow did not finish: these nodes never completed. */
export const FlowIncomplete = failure("flow-incomplete", { pending: Schema.Array(Schema.String) })
/**
 * The document names a deviation this interpreter would have to EMIT and cannot.
 * `step` is a serde `Option` with no skip, so it rides as an explicit `null`.
 */
export const DeviationUnimplemented = failure("deviation-unimplemented", {
  deviation: Schema.String,
  kind: Schema.String,
  step: nullable(Schema.String),
  reason: Schema.String
})
/** The document holds an `inject` and the lane supplied no injector. */
export const InjectorMissing = failure("injector-missing", {
  node: Schema.String,
  action: Schema.String
})
/** The lane bound no number for an identity the run needs. */
export const IdentityUnbound = failure("identity-unbound", {
  site: Schema.String,
  identity: Schema.String,
  detail: Schema.String
})
/** A socket closed or errored under the run, in the transport's own words. */
export const TransportClosed = failure("transport-closed", {
  actor: Schema.String,
  detail: Schema.String
})
/** The recording could not be written — losing the run's evidence fails the run. */
export const RecordingFailed = failure("recording-failed", { detail: Schema.String })
/** The run made no progress and virtual time stopped advancing. */
export const RunStalled = failure("run-stalled", {
  phase: Schema.String,
  waiting_on: Schema.Array(Schema.String)
})
/** The run body never returned (a panic, an assertion, the RFC gate). */
export const RunUnwound = failure("run-unwound", { detail: Schema.String })
/** The document states an RFC violation the SYSTEM UNDER TEST emits, and no detector decides it yet. */
export const RfcViolationUnverified = failure("rfc-violation-unverified", {
  rule: RfcRule,
  step: Schema.String,
  emitter: Schema.String
})
/**
 * A step's retransmission ladder (§6.9) is not the one its emitter owed:
 * `declared` is what the document stated, `expected` what the ladder's pacer
 * owes — the same number on a scripted send, the RFC's rung count where the SUT
 * paced it.
 */
export const RetransmitCountMismatch = failure("retransmit-count-mismatch", {
  step: Schema.String,
  leg: Schema.String,
  declared: Schema.Int,
  expected: Schema.Int,
  observed: Schema.Int
})
/** A `cseq-override` (§11) on a message whose CSeq the stack does not choose. */
export const CseqOverrideRefused = failure("cseq-override-refused", {
  step: Schema.String,
  leg: Schema.String,
  deviation: Schema.String,
  reason: Schema.String
})
/** A `verbatim-emission` / `raw-order` step whose emission did NOT carry the stored block as held. */
export const EmissionNotPreserved = failure("emission-not-preserved", {
  step: Schema.String,
  leg: Schema.String,
  deviation: Schema.String,
  detail: Schema.String
})
/** A timer-anchored dwell the system did not measure, beyond the run's stated tolerance (§9.2). */
export const TimingOutOfTolerance = failure("timing-out-of-tolerance", {
  step: Schema.String,
  leg: Schema.String,
  declared_ms: Schema.Int,
  observed_ms: Schema.Int,
  tolerance_ms: Schema.Int
})
/** The run configuration directs a call the run does not dial. */
export const CallDirectiveUnplaced = failure("call-directive-unplaced", {
  call: Schema.String,
  detail: Schema.String
})
/** A `send` step declares `retransmits` for a message that retransmits on no timer of its own. */
export const RetransmitLadderRefused = failure("retransmit-ladder-refused", {
  step: Schema.String,
  leg: Schema.String,
  detail: Schema.String
})
/** A `must_fail` declaration (§11.2) the run did NOT produce. */
export const DeclaredFailureNotProduced = failure("declared-failure-not-produced", {
  declared: DeclaredFailure,
  step: Schema.String,
  detail: Schema.String
})

/** One thing that went wrong: the site and the evidence, under its own tag. */
export const Failure = Schema.Union([
  PlanRefused,
  ExpectTimedOut,
  UnmatchedDatagram,
  UnexpectedDatagram,
  DatagramAfterFlow,
  FinalUnacknowledged,
  CheckFailed,
  AccessorUnresolved,
  SendFailed,
  SettleTimedOut,
  BackgroundCount,
  CdrMismatch,
  FlowIncomplete,
  DeviationUnimplemented,
  InjectorMissing,
  IdentityUnbound,
  TransportClosed,
  RecordingFailed,
  RunStalled,
  RunUnwound,
  RfcViolationUnverified,
  RetransmitCountMismatch,
  CseqOverrideRefused,
  EmissionNotPreserved,
  TimingOutOfTolerance,
  CallDirectiveUnplaced,
  RetransmitLadderRefused,
  DeclaredFailureNotProduced
])
export type Failure = typeof Failure.Type

/** The flow step this failure names, where it names one. */
export const failureStep = (value: Failure): string | undefined => {
  switch (value.failure) {
    case "expect-timed-out":
    case "unmatched-datagram":
    case "send-failed":
    case "retransmit-count-mismatch":
    case "timing-out-of-tolerance":
    case "retransmit-ladder-refused":
    case "declared-failure-not-produced":
      return value.step
    case "deviation-unimplemented":
      return value.step ?? undefined
    default:
      return undefined
  }
}

/** One RFC violation the document declares, as the verdict lists it (§11.1). */
export const ViolationNote = Schema.Struct({
  rule: RfcRule,
  step: Schema.String,
  emitter: Schema.String,
  /** Whether the run's status turns on it. False for every scripted peer. */
  gating: Schema.Boolean
})
export interface ViolationNote extends Schema.Schema.Type<typeof ViolationNote> {}

/** One `must_fail` declaration, against what the run produced (§11.2). */
export const DeclaredNote = Schema.Struct({
  failure: DeclaredFailure,
  step: Schema.String,
  derived_from: RfcRule,
  observed: Schema.optionalKey(Failure),
  /**
   * The declared datagram as the RECORDING holds it, where it arrived after the
   * script had ended. A declaration is satisfied by either field.
   */
  recorded: Schema.optionalKey(Schema.String)
})
export interface DeclaredNote extends Schema.Schema.Type<typeof DeclaredNote> {}

/** What the generic close put on the wire for one leg (§11.2). */
export const CloseOwed = Schema.Literals(["answer", "ack", "bye", "cancel"])
export type CloseOwed = typeof CloseOwed.Type

/** One act of the generic close: what a scripted leg emitted once its script had ended. */
export const CloseAct = Schema.Struct({
  leg: Schema.String,
  owed: CloseOwed,
  sent: Schema.String
})
export interface CloseAct extends Schema.Schema.Type<typeof CloseAct> {}

/**
 * A script a run that COULD NOT GO ON ended, and the generic close that
 * terminated its call instead (§11.2). Polarity-free: a positive run that cannot
 * go on is abandoned and closed exactly like a negative one.
 */
export const Abandoned = Schema.Struct({
  leg: Schema.optionalKey(Schema.String),
  step: Schema.optionalKey(Schema.String),
  pending: Schema.optionalKey(Schema.Array(Schema.String)),
  closed: Schema.optionalKey(Schema.Array(CloseAct))
})
export interface Abandoned extends Schema.Schema.Type<typeof Abandoned> {}

/**
 * One classified check that did not hold and did not gate (§9.1). The check IS
 * evaluated — a downgrade is not a skip — and its finding lands here instead of
 * in `failures`.
 */
export const Informative = Schema.Struct({
  class: CheckClass,
  finding: Failure
})
export interface Informative extends Schema.Schema.Type<typeof Informative> {}

/**
 * One check a lane's declared known bug stood down. The datagram matched and
 * the run went on; the status is computed from `failures` alone, and a run that
 * waived nothing carries none — so a reader can tell a clean match from a match
 * bought by a waiver.
 */
export const Waived = Schema.Struct({
  bug: KnownBug,
  finding: Failure
})
export interface Waived extends Schema.Schema.Type<typeof Waived> {}

/** One step's retransmission ladder, as the run counted it (§6.9). */
export const LadderSide = Schema.Literals(["send", "expect"])
export type LadderSide = typeof LadderSide.Type

export const RetransmitNote = Schema.Struct({
  step: Schema.String,
  leg: Schema.String,
  /** Which side paced the ladder: `send` is the scripted peer's, `expect` the SUT's. */
  side: LadderSide,
  declared: Schema.Int,
  observed: Schema.Int,
  /** The document's own gaps for this step (§6.9); absent where it stated none. */
  intervals_ms: Schema.optionalKey(Schema.Array(Schema.Int)),
  /** The claimed datagram to the closer that ended the ladder. */
  dwell_us: Schema.optionalKey(Schema.Int),
  /**
   * The rungs an RFC-paced ladder of this message's class puts inside the
   * window it had — the dwell, or the run itself where nothing closed it. The
   * EXPECT side only: a send ladder paced itself.
   */
  rfc_rungs: Schema.optionalKey(Schema.Int)
})
export interface RetransmitNote extends Schema.Schema.Type<typeof RetransmitNote> {}

/**
 * One timer-anchored dwell, declared against observed (§9.2). Every completed
 * `timer_linked` expect is listed: a window that hides what it swallowed is a
 * window nobody can audit.
 */
export const TimingNote = Schema.Struct({
  step: Schema.String,
  leg: Schema.String,
  declared_ms: Schema.Int,
  observed_ms: Schema.Int,
  /** Observed minus declared: negative fired early, positive fired late. */
  delta_ms: Schema.Int,
  tolerance_ms: Schema.Int
})
export interface TimingNote extends Schema.Schema.Type<typeof TimingNote> {}

/** The run's verdict, as `verdict.json` in the run bundle. */
export const RunVerdict = Schema.Struct({
  case: Schema.String,
  lane: Schema.String,
  status: VerdictStatus,
  failures: Schema.optionalKey(Schema.Array(Failure)),
  failed_step: Schema.optionalKey(Schema.String),
  branches: Schema.optionalKey(Schema.Record(Schema.String, Schema.String)),
  completed_steps: Schema.optionalKey(Schema.Array(Schema.String)),
  released_optional: Schema.optionalKey(Schema.Array(Schema.String)),
  retired: Schema.optionalKey(Schema.Array(Schema.String)),
  rfc_violations: Schema.optionalKey(Schema.Array(ViolationNote)),
  must_fail: Schema.optionalKey(Schema.Array(DeclaredNote)),
  tolerated: Schema.optionalKey(Schema.Array(Failure)),
  abandoned: Schema.optionalKey(Abandoned),
  informative: Schema.optionalKey(Schema.Array(Informative)),
  waived: Schema.optionalKey(Schema.Array(Waived)),
  retransmits: Schema.optionalKey(Schema.Array(RetransmitNote)),
  timings: Schema.optionalKey(Schema.Array(TimingNote))
})
export interface RunVerdict extends Schema.Schema.Type<typeof RunVerdict> {}

/**
 * Whether the run PASSED — green, or green-as-negative (§11.2). Both are
 * outcomes a case can be built on; neither carries an open failure.
 */
export const verdictPassed = (verdict: RunVerdict): boolean =>
  (verdict.status === "ok" || verdict.status === "ok-negative") && (verdict.failures ?? []).length === 0

/**
 * The RFC-violation census report (`sipflow --rfc-census`), mirroring
 * `sip_pcap::rfc`'s report types: one hit per rule a detector decided off the
 * wire, plus the tallies of the sweep that found them.
 *
 * Hand-mirrored, as `./lint.ts` is: the report derives `Serialize` alone
 * upstream, so no published JSON Schema pins it and the struct declarations are
 * the only source of truth.
 *
 * An unknown field is a LOAD failure and not a shrug: it means the detector
 * moved ahead of this consumer, and a reader that ignored the difference would
 * decide off a shape it does not understand.
 *
 * A rule the report does not NAME is the opposite case and is stated, not
 * refused: the vocabulary grows, and a run taken before a rule existed measured
 * it nowhere. What that costs is the reader's to weigh — see
 * {@link measuredRules} — because only the reader knows which rules it acts on.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"
import { RFC_RULES, type RfcRule } from "./violation.js"

/** What every hit states, whichever rule decided it. */
const HitHead = {
  /** Path of the flows document the sweep read. */
  document: Schema.String,
  /** The source capture's file name — the key every consumer records against. */
  capture: Schema.String,
  group: Schema.Int,
  leg: Schema.Int,
  call_id: Schema.String,
  /** The charged endpoint, `ip:port`. */
  emitter: Schema.String,
  /**
   * The census's own group-topology reading of the emitter's side. EVIDENCE: a
   * consumer that owns a deployment's address set places the endpoint from
   * that, never from this.
   */
  emitter_role: Schema.Literals(["platform", "peer", "undetermined"]),
  /**
   * The side the SUT set stated to `sipflow --rfc --sut` placed the emitter
   * on. Present only on a review taken with a stated set; a sweep carries none.
   */
  side: Schema.optionalKey(Schema.Literals(["platform", "peer"])),
  taker: Schema.String,
  cseq: Schema.Int,
  relayed: Schema.Boolean
}

/** `no-200-after-cancel`: the CANCEL taken and the 2xx sent after it. */
export const CancelHit = Schema.Struct({
  ...HitHead,
  rule: Schema.Literal("no-200-after-cancel"),
  cancel_msg: Schema.Int,
  cancel_hop: Schema.Int,
  cancel_ts_us: Schema.Int,
  response_msg: Schema.Int,
  response_hop: Schema.Int,
  response_ts_us: Schema.Int,
  status: Schema.Int,
  gap_us: Schema.Int
})
export interface CancelHit extends Schema.Schema.Type<typeof CancelHit> {}

/** `unacked-reliable-provisional`: the provisional taken and never PRACKed. */
export const UnackedHit = Schema.Struct({
  ...HitHead,
  rule: Schema.Literal("unacked-reliable-provisional"),
  provisional_msg: Schema.Int,
  provisional_hop: Schema.Int,
  provisional_ts_us: Schema.Int,
  rseq: Schema.Int,
  status: Schema.Int,
  window_us: Schema.Int
})
export interface UnackedHit extends Schema.Schema.Type<typeof UnackedHit> {}

/** `no-ack-to-dialog-creating-2xx`: the 2xx taken and never ACKed. */
export const NoAckHit = Schema.Struct({
  ...HitHead,
  rule: Schema.Literal("no-ack-to-dialog-creating-2xx"),
  final_msg: Schema.Int,
  final_hop: Schema.Int,
  final_ts_us: Schema.Int,
  to_tag: Schema.String,
  status: Schema.Int,
  retransmits: Schema.Int,
  window_us: Schema.Int,
  emitter_window_us: Schema.Int,
  /** Absent where the dialog was never torn down at this vantage. */
  bye_after_us: Schema.optionalKey(Schema.Int),
  bye_by: Schema.optionalKey(Schema.String)
})
export interface NoAckHit extends Schema.Schema.Type<typeof NoAckHit> {}

/** `no-cancel-after-final`: the final taken and ACKed, and the CANCEL sent after it. */
export const LateCancelHit = Schema.Struct({
  ...HitHead,
  rule: Schema.Literal("no-cancel-after-final"),
  cancel_msg: Schema.Int,
  cancel_hop: Schema.Int,
  cancel_ts_us: Schema.Int,
  invite_msg: Schema.Int,
  invite_hop: Schema.Int,
  invite_ts_us: Schema.Int,
  final_msg: Schema.Int,
  final_hop: Schema.Int,
  final_ts_us: Schema.Int,
  final_status: Schema.Int,
  ack_msg: Schema.Int,
  ack_hop: Schema.Int,
  ack_ts_us: Schema.Int,
  since_final_us: Schema.Int
})
export interface LateCancelHit extends Schema.Schema.Type<typeof LateCancelHit> {}

/**
 * `second-answer-repeats-the-first`: the binding answer the dialog already
 * carried, and the second one that states another transport plan.
 */
export const SecondAnswerHit = Schema.Struct({
  ...HitHead,
  rule: Schema.Literal("second-answer-repeats-the-first"),
  second_answer_msg: Schema.Int,
  second_answer_hop: Schema.Int,
  second_answer_ts_us: Schema.Int,
  offer_msg: Schema.Int,
  first_answer_msg: Schema.Int,
  /** Each answer's plan: the session `c=`, then one row per stream. */
  first_plan: Schema.Array(Schema.String),
  second_plan: Schema.Array(Schema.String)
})
export interface SecondAnswerHit extends Schema.Schema.Type<typeof SecondAnswerHit> {}

/** One hit, internally tagged on `rule`. */
export const CensusHit = Schema.Union([
  CancelHit,
  UnackedHit,
  NoAckHit,
  LateCancelHit,
  SecondAnswerHit
])
export type CensusHit = typeof CensusHit.Type

/** What one rule found across the whole sweep. */
export const RuleTally = Schema.Struct({
  hits: Schema.Int,
  documents: Schema.Int,
  occasions: Schema.Int,
  decided: Schema.Int,
  by_role: Schema.Record(Schema.String, Schema.Int),
  /** Hits by stated side; present only when the run stated a SUT set. */
  by_side: Schema.optionalKey(Schema.Record(Schema.String, Schema.Int)),
  relayed: Schema.Int,
  buckets: Schema.Record(Schema.String, Schema.Int)
})
export interface RuleTally extends Schema.Schema.Type<typeof RuleTally> {}

/** One document the sweep could not read, and why. */
export const CensusFailure = Schema.Struct({
  document: Schema.String,
  reason: Schema.String
})
export interface CensusFailure extends Schema.Schema.Type<typeof CensusFailure> {}

/**
 * One sweep, modelled in full.
 *
 * `rules` is PARTIAL over the vocabulary: a run states a tally for each rule its
 * detector held, and a rule minted after the sweep has no line here. An unknown
 * rule is still refused, and so is a malformed tally.
 */
export const CensusReport = Schema.Struct({
  documents: Schema.Int,
  groups: Schema.Int,
  legs: Schema.Int,
  messages: Schema.Int,
  rules: Schema.Record(Schema.Literals(RFC_RULES), Schema.optionalKey(RuleTally)),
  hits: Schema.Array(CensusHit),
  failures: Schema.Array(CensusFailure),
  /** The SUT set every hit's `side` was placed by; absent on a sweep. */
  sut: Schema.optionalKey(
    Schema.Struct({
      addresses: Schema.Array(Schema.String),
      decided_by: Schema.Literals(["stated", "mint-point"])
    })
  )
})
export interface CensusReport extends Schema.Schema.Type<typeof CensusReport> {}

/**
 * The rules this run measured. A rule absent from it was measured NOWHERE, which
 * on the hit list reads exactly like a corpus that carries none of it — so a
 * consumer that acts on a rule's zero asks this first.
 */
export const measuredRules = (report: CensusReport): ReadonlySet<RfcRule> =>
  new Set(RFC_RULES.filter((rule) => report.rules[rule] !== undefined))

export const decodeCensusReport = Schema.decodeUnknownEffect(CensusReport, STRICT)
export const decodeCensusReportSync = Schema.decodeUnknownSync(CensusReport, STRICT)

/** Parse a census report from its text. */
export const parseCensusReport = (text: string) =>
  Effect.suspend(() => decodeCensusReport(JSON.parse(text) as unknown))

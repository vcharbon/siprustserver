/**
 * `rfc_violations` (`PCAP2TEST_PIVOT_V3.md` §11.1), mirroring
 * `pivot_schema::violation`: a rule a message the flow ALREADY carries breaks,
 * stated as a fact about the run.
 *
 * Distinct from `deviations` (§11), which changes what an emission looks like. A
 * violation here is behavioural: the message is byte-compliant and its TIMING or
 * its context is what breaks the rule.
 *
 * `rule` is any rule of the validator — closed over the rules with a body, each
 * decided alike off a capture (the census) and off a run (the live audit). It
 * is `rfc_rules::RuleId::ALL` position for position.
 */
import * as Schema from "effect/Schema"

/** The token naming the system under test as an emitter. */
export const SUT_EMITTER = "sut"

/**
 * The rules this format can state: every rule of the validator, by its token —
 * `rfc_rules::RuleId::ALL` in its order, pinned against
 * `crates/pivot-schema/tests/fixtures/rfc-rule-tokens.txt` by `test/violation.test.ts`.
 */
export const RFC_RULES = [
  "ack-body-after-complete-offer-answer",
  "ack-cseq-matches-invite",
  "ack-preserves-invite-route",
  "ack-require-subset-of-invite",
  "allow-supported-on-invite",
  "answer-m-line-count-matches-offer",
  "answer-media-type-matches-offer",
  "answer-stream-matches-offer",
  "answer-t-line-equals-offer",
  "branch-prefix",
  "c0-port-non-zero",
  "cancel-after-1xx",
  "cancel-cseq-method",
  "cancel-request-uri",
  "cancel-route-echoes-invite",
  "cancel-via-branch",
  "concurrent-re-invite-500-or-491",
  "contact-presence",
  "content-length",
  "content-type",
  "cseq-in-dialog-order",
  "delay-2xx-on-unacked-reliable-1xx-with-sdp",
  "delayed-offer-answered-in-ack",
  "dialog-call-id-stable",
  "direction-pair-valid",
  "failed-reinvite-tears-down-dialog",
  "final-2xx-answers-the-offer",
  "from-tag-presence",
  "in-dialog-from-tag",
  "in-dialog-to-tag",
  "max-forwards",
  "mid-dialog-route",
  "mid-dialog-tags",
  "mid-dialog-uri",
  "mid-dialog-wire-destination",
  "no-100rel-require-on-non-invite",
  "no-1xx-after-final",
  "no-200-after-cancel",
  "no-ack-to-dialog-creating-2xx",
  "no-bye-outside-or-early-dialog",
  "no-cancel-after-final",
  "no-contact-on-bye",
  "no-new-offer-while-offer-pending",
  "no-new-reliable-1xx-after-final",
  "no-overlapping-reliable-provisionals",
  "no-prack-of-100-trying",
  "no-prack-of-out-of-order-rseq",
  "no-re-invite-while-invite-in-progress",
  "no-record-route-from-ua",
  "no-reliable-1xx-on-in-dialog",
  "no-require-on-cancel-or-ack",
  "no-target-404",
  "no-to-tag-on-initial-request",
  "non-contiguous-rseq",
  "options-response-echoes",
  "payload-type-mapping-stable",
  "peer-uri-stable",
  "prack-2xx-or-481",
  "prack-answers-1xx-offer",
  "proxy-100-trying-not-forwarded",
  "proxy-100-within-grace",
  "rack-without-known-invite",
  "re-offer-m-line-count-monotonic",
  "record-route-placement",
  "register-no-route-set",
  "rejected-stream-minimal-answer",
  "reliable-1xx-headers",
  "reliable-needs-client-opt-in",
  "require-reliable-1xx-on-require",
  "response-correlation",
  "response-cseq-matches-transaction",
  "response-echoes-request-via",
  "rport-echo",
  "rung-byte-identical",
  "sdp-body-parseable",
  "sdp-origin-continuity",
  "second-answer-repeats-the-first",
  "serial-register",
  "single-final-per-server-txn",
  "strict-route-rewrite-handled",
  "strict-route-shuffle-on-send",
  "tag-consistency",
  "taken-final-ends-invite-txn",
  "to-tag-presence",
  "unacked-2xx-not-cleared",
  "unacked-invite-non-2xx-final",
  "unacked-reliable-provisional",
  "unknown-dialog-481",
  "unmatched-prack-proxied",
  "unsupported-415-accepts",
  "unsupported-extension-420",
  "unsupported-extension-421",
  "unsupported-method-405-allow",
  "zero-port-propagation"
] as const

export const RfcRule = Schema.Literals(RFC_RULES)
export type RfcRule = typeof RfcRule.Type

/** One RFC rule a message of this flow breaks. */
export const RfcViolation = Schema.Struct({
  rule: RfcRule,
  step: Schema.String,
  emitter: Schema.String,
  /**
   * On a `sut` entry only: the step of a scripted party's entry of the same rule
   * this one carries on — the captured platform relayed onward a violation that
   * party committed first. Such an entry gates nothing.
   */
  relays: Schema.optionalKey(Schema.String)
})
export interface RfcViolation extends Schema.Schema.Type<typeof RfcViolation> {}

/** Whether the system under test is the emitter. */
export const violationSutEmitted = (violation: RfcViolation): boolean => violation.emitter === SUT_EMITTER

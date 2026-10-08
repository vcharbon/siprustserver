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
  "no-200-after-cancel",
  "unacked-reliable-provisional",
  "no-ack-to-dialog-creating-2xx",
  "unacked-2xx-not-cleared",
  "rack-without-known-invite",
  "no-overlapping-reliable-provisionals",
  "non-contiguous-rseq",
  "no-prack-of-out-of-order-rseq",
  "single-final-per-server-txn",
  "cancel-route-echoes-invite",
  "cancel-after-1xx",
  "cseq-in-dialog-order",
  "response-cseq-matches-transaction",
  "ack-cseq-matches-invite",
  "mid-dialog-uri",
  "mid-dialog-route",
  "mid-dialog-wire-destination",
  "record-route-placement",
  "rport-echo",
  "allow-supported-on-invite",
  "proxy-100-trying-not-forwarded",
  "unknown-dialog-481",
  "unsupported-method-405-allow",
  "unsupported-extension-420",
  "unsupported-415-accepts",
  "unsupported-extension-421",
  "no-target-404",
  "options-response-echoes",
  "ack-require-subset-of-invite",
  "ack-preserves-invite-route",
  "strict-route-rewrite-handled",
  "serial-register",
  "register-no-route-set",
  "concurrent-re-invite-500-or-491",
  "no-bye-outside-or-early-dialog",
  "no-re-invite-while-invite-in-progress",
  "proxy-100-within-grace",
  "unacked-invite-non-2xx-final",
  "failed-reinvite-tears-down-dialog",
  "no-1xx-after-final",
  "require-reliable-1xx-on-require",
  "reliable-needs-client-opt-in",
  "no-reliable-1xx-on-in-dialog",
  "unmatched-prack-proxied",
  "prack-2xx-or-481",
  "delay-2xx-on-unacked-reliable-1xx-with-sdp",
  "prack-accepted-after-final",
  "no-new-reliable-1xx-after-final",
  "no-prack-of-100-trying",
  "prack-answers-1xx-offer",
  "ack-body-after-complete-offer-answer",
  "final-2xx-answers-the-offer",
  "delayed-offer-answered-in-ack",
  "second-answer-repeats-the-first",
  "answer-stream-matches-offer",
  "sdp-origin-continuity",
  "no-new-offer-while-offer-pending",
  "answer-m-line-count-matches-offer",
  "answer-t-line-equals-offer",
  "answer-media-type-matches-offer",
  "direction-pair-valid",
  "rejected-stream-minimal-answer",
  "re-offer-m-line-count-monotonic",
  "zero-port-propagation",
  "payload-type-mapping-stable",
  "branch-prefix",
  "max-forwards",
  "content-length",
  "content-type",
  "contact-presence",
  "no-contact-on-bye",
  "to-tag-presence",
  "no-record-route-from-ua",
  "response-echoes-request-via",
  "response-correlation",
  "mid-dialog-tags",
  "peer-uri-stable",
  "dialog-call-id-stable",
  "cancel-request-uri",
  "cancel-via-branch",
  "tag-consistency",
  "no-100rel-require-on-non-invite",
  "reliable-1xx-headers",
  "cancel-cseq-method",
  "no-to-tag-on-initial-request",
  "in-dialog-to-tag",
  "no-require-on-cancel-or-ack",
  "strict-route-shuffle-on-send",
  "sdp-body-parseable",
  "c0-port-non-zero",
  "no-cancel-after-final",
  "rung-byte-identical"
] as const

export const RfcRule = Schema.Literals(RFC_RULES)
export type RfcRule = typeof RfcRule.Type

/** One RFC rule a message of this flow breaks. */
export const RfcViolation = Schema.Struct({
  rule: RfcRule,
  step: Schema.String,
  emitter: Schema.String
})
export interface RfcViolation extends Schema.Schema.Type<typeof RfcViolation> {}

/** Whether the system under test is the emitter — the one case that gates. */
export const violationSutEmitted = (violation: RfcViolation): boolean => violation.emitter === SUT_EMITTER

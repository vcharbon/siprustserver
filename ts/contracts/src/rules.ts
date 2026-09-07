/**
 * The correlation-rule file (`rule-format-v0.md`), mirroring
 * `pivot_schema::rules`: how calls join into cases.
 *
 * This file is the MIDDLE of three layers. In-dialog assembly (Call-ID + tags to
 * dialog, dialogs to legs, B2BUA pairing) is deterministic SIP semantics done
 * before any rule runs. Interpretation — what a chain MEANS — happens after,
 * over the neutral groups. A rule carries NO meaning: its name is an evidence
 * label.
 *
 * Five kinds, each hard-coding its own anchor, internally tagged on `kind` in
 * kebab-case. The extension path is a SIXTH kind with its own small matcher,
 * never a more general existing one.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { STRICT } from "./strict.js"

/** The `version` this contract models. */
export const RULE_FILE_VERSION = 0

/**
 * A call-level identity set a `retry` rule may match on. Two calls match when
 * their digits-normalized sets intersect.
 */
export const MatchField = Schema.Literals(["from-user", "to-user", "ruri-user"])
export type MatchField = typeof MatchField.Type

/** Derived Call-ID relationship. Anchor: each call's Call-ID, any leg. No window. */
export const CallIdRule = Schema.Struct({
  kind: Schema.Literal("call-id"),
  name: Schema.String,
  left: Schema.String,
  right: Schema.String
})
export interface CallIdRule extends Schema.Schema.Type<typeof CallIdRule> {}

/** Shared token in a header of the initial INVITE. Anchor: each call's first INVITE. */
export const HeaderKeyRule = Schema.Struct({
  kind: Schema.Literal("header-key"),
  name: Schema.String,
  header: Schema.String,
  pattern: Schema.String,
  window_ms: Schema.optionalKey(Schema.Int)
})
export interface HeaderKeyRule extends Schema.Schema.Type<typeof HeaderKeyRule> {}

/**
 * Reroute / failover chain, CALL-LEVEL: whole-call facts only. A final joins the
 * NEXT candidate INVITE, so a chain of N attempts is N-1 ordered joins.
 * `match` absent: the rule fires only between calls already grouped by stronger
 * evidence, so it never joins strangers.
 */
export const RetryRule = Schema.Struct({
  kind: Schema.Literal("retry"),
  name: Schema.String,
  finals: Schema.Array(Schema.String),
  window_ms: Schema.Int,
  match: Schema.optionalKey(Schema.Array(MatchField))
})
export interface RetryRule extends Schema.Schema.Type<typeof RetryRule> {}

/** Attended-transfer linkage. The `Replaces` header is the evidence, so no fields. */
export const ReplacesRule = Schema.Struct({
  kind: Schema.Literal("replaces"),
  name: Schema.String
})
export interface ReplacesRule extends Schema.Schema.Type<typeof ReplacesRule> {}

/** REFER-initiated call: the right call's INVITE reaches the `Refer-To` target user. */
export const ReferRule = Schema.Struct({
  kind: Schema.Literal("refer"),
  name: Schema.String,
  window_ms: Schema.Int
})
export interface ReferRule extends Schema.Schema.Type<typeof ReferRule> {}

/** One rule. `name` is unique and shows up in the evidence a join carries. */
export const Rule = Schema.Union([CallIdRule, HeaderKeyRule, RetryRule, ReplacesRule, ReferRule])
export type Rule = typeof Rule.Type

/** A correlation rule file. The rules are tried in file order. */
export const RuleFile = Schema.Struct({
  version: Schema.Int,
  rules: Schema.Array(Rule)
})
export interface RuleFile extends Schema.Schema.Type<typeof RuleFile> {}

export const decodeRuleFile = Schema.decodeUnknownEffect(RuleFile, STRICT)
export const decodeRuleFileSync = Schema.decodeUnknownSync(RuleFile, STRICT)
export const encodeRuleFile = Schema.encodeUnknownSync(RuleFile, STRICT)

/** Parse a rule file from its text. */
export const parseRuleFile = (text: string) => Effect.suspend(() => decodeRuleFile(JSON.parse(text) as unknown))

/** The rule's key-bearing regex fields, as `[field, regex]`. */
export const ruleKeyRegexes = (rule: Rule): Array<[string, string]> => {
  switch (rule.kind) {
    case "call-id":
      return [
        ["left", rule.left],
        ["right", rule.right]
      ]
    case "header-key":
      return [["pattern", rule.pattern]]
    default:
      return []
  }
}

/**
 * The checks the shape alone cannot state: unique names, and every key-bearing
 * regex carrying its `(?<key>…)` group. One message per offence — the same list
 * `RuleFile::violations` produces.
 */
export const ruleFileViolations = (file: RuleFile): Array<string> => {
  const seen = new Set<string>()
  const out: Array<string> = []
  for (const rule of file.rules) {
    if (seen.has(rule.name)) out.push(`duplicate rule name ${JSON.stringify(rule.name)}`)
    seen.add(rule.name)
    for (const [field, regex] of ruleKeyRegexes(rule)) {
      if (!regex.includes("(?<key>")) {
        out.push(`rule ${JSON.stringify(rule.name)}: ${JSON.stringify(field)} needs a (?<key>…) group`)
      }
    }
  }
  return out
}

/** Throw on the first semantic offence, for the callers that want a hard stop. */
export const validateRuleFile = (file: RuleFile): void => {
  const found = ruleFileViolations(file)
  if (found.length > 0) throw new Error(found.join("; "))
}

/**
 * Every header a rule file names — the `--emit-headers` allow-list the flows
 * emitter is asked for, so a rule can never read a header the document does not
 * project.
 */
export const headersNamedBy = (rules: ReadonlyArray<Rule>): Array<string> => [
  ...new Set(rules.flatMap((rule) => (rule.kind === "header-key" ? [rule.header] : [])))
]

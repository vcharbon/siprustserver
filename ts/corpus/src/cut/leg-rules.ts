/**
 * The LEG RULES a cut joins a capture's legs by beside the Call-ID derivation:
 * the `header-pair` rules of a correlation rule file (`@sip/contracts`
 * `Rules`), and the base's default set, `data/default.rules.json`.
 *
 * The default holds one rule, Asterisk's convention: the leg it dials carries
 * the caller leg's Call-ID in an `X-CID` header. The flows document a cut reads
 * under rules projects every header they name ({@link headersOf}), which the
 * extraction is asked for (`Tools.Interface.emitHeaders`).
 */
import { Rules } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"
import { fileURLToPath } from "node:url"

/** The base's default rule set, the one the base generator reads where its caller names none. */
export const DEFAULT_RULES = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "data", "default.rules.json")

/** The rule file cannot be read as leg rules; `reason` says why. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("LegRules.Unreadable", {
  file: Schema.String,
  reason: Schema.String
}) {
  override get message(): string {
    return `the rule file ${this.file} does not read: ${this.reason}`
  }
}

/**
 * The rules of the rule file at `file`, every one a `header-pair` rule: a file
 * that does not decode, breaks a check its shape cannot state, or holds a rule
 * of another kind, which no cut reads, is refused.
 */
export const legRulesOf = Effect.fn("LegRules.legRulesOf")(function*(file: string) {
  const unreadable = (reason: unknown) => new Unreadable({ file, reason: String(reason) })
  const text = yield* (yield* FileSystem.FileSystem).readFileString(file).pipe(Effect.mapError(unreadable))
  const ruleFile = yield* Rules.parseRuleFile(text).pipe(Effect.mapError(unreadable))
  const violations = Rules.ruleFileViolations(ruleFile)
  if (violations.length > 0) return yield* unreadable(violations.join("; "))
  const others = ruleFile.rules.filter((rule) => rule.kind !== "header-pair").map((rule) => `${rule.name} (${rule.kind})`)
  if (others.length > 0) return yield* unreadable(`a cut joins legs by header-pair rules alone, not ${others.join(", ")}`)
  return ruleFile.rules.filter((rule): rule is Rules.HeaderPairRule => rule.kind === "header-pair")
})

/** The headers `rules` read: the `Tools.Interface.emitHeaders` of a deployment cutting under them. */
export const headersOf = (rules: ReadonlyArray<Rules.HeaderPairRule>): ReadonlyArray<string> => Rules.headersNamedBy(rules)

/**
 * The LEG RULES a cut joins a capture's legs by beside the Call-ID derivation:
 * the `header-pair` rules of a correlation rule file (`@sip/contracts`
 * `Rules`), and the base's default set, `data/default.rules.json`.
 *
 * The default holds one rule, a convention seen in Asterisk dialplans: the leg
 * the dialplan dials carries the caller leg's Call-ID in an `X-CID` header. The
 * flows document a cut reads under rules projects every header they name but
 * `Call-ID`, the leg's own ({@link headersOf}), which the extraction is asked
 * for (`Tools.Interface.emitHeaders`).
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

/** Why each side's pattern of `rule` does not compile as the cut runs it, one line per side. */
const uncompiled = (rule: Rules.HeaderPairRule): ReadonlyArray<string> =>
  (["left", "right"] as const).flatMap((side) => {
    try {
      new RegExp(rule[side].pattern)
      return []
    } catch (error) {
      return [`rule ${JSON.stringify(rule.name)}: "${side}.pattern" does not compile: ${error instanceof Error ? error.message : String(error)}`]
    }
  })

/**
 * The rules of the rule file at `file`, every one a `header-pair` rule whose
 * patterns compile: a file that does not decode, breaks a check its shape
 * cannot state, holds a rule of another kind, which no cut reads, or a pattern
 * that does not compile is refused.
 */
export const legRulesOf = Effect.fn("LegRules.legRulesOf")(function*(file: string) {
  const unreadable = (reason: unknown) => new Unreadable({ file, reason: String(reason) })
  const text = yield* (yield* FileSystem.FileSystem).readFileString(file).pipe(Effect.mapError(unreadable))
  const ruleFile = yield* Rules.parseRuleFile(text).pipe(Effect.mapError(unreadable))
  const violations = Rules.ruleFileViolations(ruleFile)
  if (violations.length > 0) return yield* unreadable(violations.join("; "))
  const others = ruleFile.rules.filter((rule) => rule.kind !== "header-pair").map((rule) => `${rule.name} (${rule.kind})`)
  if (others.length > 0) return yield* unreadable(`a cut joins legs by header-pair rules alone, not ${others.join(", ")}`)
  const rules = ruleFile.rules.filter((rule): rule is Rules.HeaderPairRule => rule.kind === "header-pair")
  const broken = rules.flatMap(uncompiled)
  if (broken.length > 0) return yield* unreadable(broken.join("; "))
  return rules
})

/**
 * The headers `rules` read off a projection, `Call-ID` left out: the
 * `Tools.Interface.emitHeaders` of a deployment cutting under them.
 */
export const headersOf = (rules: ReadonlyArray<Rules.HeaderPairRule>): ReadonlyArray<string> => Rules.headersNamedBy(rules)

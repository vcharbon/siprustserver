/**
 * What the roster loader refuses: every way a set of refusal rules can lie about
 * itself, caught at load time rather than on a corpus.
 */
import { describe, expect, it } from "vitest"
import { CUT_RULES } from "../src/cut.js"
import { DOCUMENT_RULES } from "../src/document-rules.js"
import { policyWith, refusalRoster, tieredRoster } from "../src/policy.js"
import { checkRefusalRoster, refusalRuleIds } from "../src/refusal-roster.js"
import type { RuleIdentity } from "../src/refusal-rule.js"

const rule = (id: string, subject: RuleIdentity["subject"]): RuleIdentity => ({
  id,
  subject,
  disposition: "refuses"
})

const check = (rules: ReadonlyArray<RuleIdentity>, deploymentFree = false) => () =>
  checkRefusalRoster(rules, { where: "under test", deploymentFree })

describe("the refusal roster loader", () => {
  it("accepts a roster whose tokens agree with their subjects", () => {
    expect(
      check([rule("source-a", "source"), rule("egress-b", "egress"), rule("sut-c", "sut")])
    ).not.toThrow()
  })

  it("refuses two rules sharing one token", () => {
    expect(check([rule("source-a", "source"), rule("source-a", "source")])).toThrow(
      /share the token 'source-a'/
    )
  })

  it("refuses a token whose prefix names another party", () => {
    expect(check([rule("source-a", "egress")])).toThrow(/carries no 'egress-' prefix/)
  })

  it("refuses a deployment subject inside a deployment-free roster", () => {
    expect(check([rule("sut-a", "sut")], true)).toThrow(/deployment-free roster cannot state/)
  })

  it("leaves the two tokens that predate the taxonomy alone", () => {
    expect(
      check([rule("missing-upstream-leg", "source"), rule("refer-replaces-out-of-scope", "scope")])
    ).not.toThrow()
  })
})

describe("the pipeline's own rosters", () => {
  it("states one token per rule, none of them a deployment's", () => {
    const rules = [...CUT_RULES, ...DOCUMENT_RULES]
    expect(refusalRuleIds(rules).size).toBe(rules.length)
    expect(rules.every((r) => r.subject !== "sut")).toBe(true)
  })

  it("gives the two ACK holes a token each, so a replay can falsify one alone", () => {
    expect(refusalRuleIds(DOCUMENT_RULES)).toContain("source-ack-not-captured")
    expect(refusalRuleIds(DOCUMENT_RULES)).toContain("source-actor-ack-not-captured")
  })
})

describe("the roster in its tiers", () => {
  it("names the same rules as the flat roster, in the same order", () => {
    const policy = policyWith({})
    const tiers = tieredRoster(policy)
    expect(refusalRoster(policy).map((r) => r.id)).toEqual(
      [...tiers.capture, ...tiers.case, ...tiers.document].map((r) => r.id)
    )
  })

  it("carries each rule's PREDICATE, which the flat roster cannot", () => {
    const tiers = tieredRoster(policyWith({}))
    for (const rule of [...tiers.capture, ...tiers.case, ...tiers.document]) {
      expect(typeof rule.refuses).toBe("function")
    }
  })

  it("puts the cut's own rules ahead of the deployment's, at the capture tier", () => {
    const tiers = tieredRoster(policyWith({}))
    expect(tiers.capture.map((r) => r.id).slice(0, CUT_RULES.length)).toEqual(
      CUT_RULES.map((r) => r.id)
    )
  })
})

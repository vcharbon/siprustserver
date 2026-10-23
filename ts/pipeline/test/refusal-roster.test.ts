/**
 * What the roster loader refuses: every way a set of refusal rules can lie about
 * itself, caught at load time rather than on a corpus.
 */
import { describe, expect, it } from "vitest"
import { CAPTURE_RULES } from "../src/capture-rules.js"
import { CASE_RULES } from "../src/case-rules.js"
import { CUT_RULES } from "../src/cut.js"
import { DOCUMENT_RULES } from "../src/document-rules.js"
import { policyWith, refusalRoster, tieredRoster } from "../src/policy.js"
import { checkRefusalRoster, refusalRuleIds } from "../src/refusal-roster.js"
import type { RuleIdentity } from "../src/refusal-rule.js"

const rule = (id: string, subject: RuleIdentity["subject"], why = "a slice of the call cannot run"): RuleIdentity => ({
  id,
  subject,
  disposition: "refuses",
  why
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

  it("refuses a rule that states no one-line reason", () => {
    expect(check([rule("source-a", "source", "")])).toThrow(/'source-a' states no one-line reason/)
    expect(check([rule("source-a", "source", "two\nlines")])).toThrow(/'source-a' states no one-line reason/)
  })

  it("leaves the two tokens that predate the taxonomy alone", () => {
    expect(
      check([rule("missing-upstream-leg", "source"), rule("refer-replaces-out-of-scope", "scope")])
    ).not.toThrow()
  })
})

describe("the pipeline's own rosters", () => {
  it("states one token per rule, each a slice that cannot run, none of them a deployment's", () => {
    const rules = [...CUT_RULES, ...CAPTURE_RULES, ...CASE_RULES, ...DOCUMENT_RULES]
    expect(refusalRuleIds(rules).size).toBe(rules.length)
    expect(rules.filter((r) => r.subject !== "source" && r.subject !== "scope").map((r) => r.id)).toEqual([])
  })

  it("states every rule's reason on one line", () => {
    for (const rule of [...CUT_RULES, ...CAPTURE_RULES, ...CASE_RULES, ...DOCUMENT_RULES]) {
      expect(rule.why).toMatch(/^[^\n]+$/)
    }
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

  it("puts the cut's own rules, then the pipeline's capture rules, ahead of the deployment's", () => {
    const deployment = { ...CAPTURE_RULES[0]!, id: "scope-deployment-own" }
    const tiers = tieredRoster(policyWith({ refuseAtCapture: [deployment] }))
    expect(tiers.capture.map((r) => r.id)).toEqual([
      ...CUT_RULES.map((r) => r.id),
      ...CAPTURE_RULES.map((r) => r.id),
      "scope-deployment-own"
    ])
  })

  it("puts the pipeline's own case rules ahead of the deployment's, at the case tier", () => {
    const deployment = { ...CASE_RULES.find((r) => r.subject === "scope")!, id: "scope-deployment-own" }
    const tiers = tieredRoster(policyWith({ refuse: [deployment] }))
    expect(tiers.case.map((r) => r.id)).toEqual([
      ...CASE_RULES.map((r) => r.id),
      "scope-deployment-own"
    ])
  })
})

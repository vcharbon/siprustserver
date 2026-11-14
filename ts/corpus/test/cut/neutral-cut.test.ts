/**
 * The neutral cut of one capture (`src/cut/neutral-cut.ts`), the base
 * generator's step, under an assembler whose Call-ID derivation counts how
 * often it is asked, or whose leg rule counts how often it is compiled and
 * how often it reads a leg: the capture's legs are correlated once, whatever
 * the number of calls it holds, and the rule reads each leg at most once per
 * side.
 */
import { NodeServices } from "@effect/platform-node"
import type { Flows } from "@sip/contracts"
import { CaseAssembler, type HeaderPairJoin, LogicExtractor, Policy } from "@sip/pipeline"
import type { CallIdDerivation } from "@sip/pipeline/derivation"
import { neverDerives } from "@sip/pipeline/derivation"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { counting, manyCallsFlows, suffixDerives } from "../../../pipeline/test/fixtures.js"
import { countingRule, LINK_RULE, linkedCallsFlows, REF_RULE, requiring, skipping } from "../../../pipeline/test/linked-legs.js"
import { cutCapture } from "../../src/cut/neutral-cut.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "neutral-cut-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The assembler bound to a policy joining legs by `derives` and `legRules` and stating nothing else. */
const assemblerJoining = (derives: CallIdDerivation, legRules: HeaderPairJoin.LegRules) =>
  CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(Policy.policyWith({ derives, legRules }))))

/** The neutral cut of the capture `capture` whose flows document is `flows`, its legs joined by `derives` and `legRules`. */
const cutOf = (capture: string, flows: Flows.FlowsDoc, derives: CallIdDerivation, legRules: HeaderPairJoin.LegRules = requiring()) => {
  const srcDir = path.join(dir, capture, "src")
  fs.mkdirSync(srcDir, { recursive: true })
  fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify(flows))
  return Effect.runPromise(
    cutCapture({ capture, srcDir, outDir: path.join(dir, capture, "out") }).pipe(
      Effect.provide(Layer.mergeAll(NodeServices.layer, assemblerJoining(derives, legRules)))
    )
  )
}

/**
 * Each call holds one derived leg, whose one suffix that is a Call-ID of the
 * document is its base: one correlation asks the derivation once per call,
 * and a second one anywhere in the cut asks every derived leg again. A leg
 * rule is compiled once per correlation, so a second one anywhere in the cut
 * reads its patterns again; it reads each side's header once per leg it
 * indexes or looks up and once to check the projection, so a join that scans
 * the legs per leg reads them legs² times.
 */
describe("the neutral cut of a capture", () => {
  it.each([1, 3, 40])("asks the derivation about each derived leg once, over %i calls", async (calls) => {
    const counted = counting(suffixDerives)
    const cut = await cutOf(`calls-${calls}.pcap`, manyCallsFlows(calls), counted.derives)
    expect(cut.cases).toHaveLength(calls)
    expect(cut.refused).toEqual([])
    expect(counted.count()).toBe(calls)
  })

  it.each([
    ["linked-call", 1, LINK_RULE],
    ["linked-call", 3, LINK_RULE],
    ["linked-call", 40, LINK_RULE],
    ["parent-ref", 1, REF_RULE],
    ["parent-ref", 3, REF_RULE],
    ["parent-ref", 40, REF_RULE]
  ] as const)("joins the legs the header-pair rule %s links in one correlation reading each leg once per side, over %i calls", async (_, calls, rule) => {
    const counted = countingRule(rule)
    const legs = 2 * calls
    const cut = await cutOf(`linked-${rule.name}-${calls}.pcap`, linkedCallsFlows(calls, rule), neverDerives, requiring(counted.rule))
    expect(cut.cases).toHaveLength(calls)
    expect(cut.refused, "every callee leg joined to the caller leg it names").toEqual([])
    expect(counted.patterns(), "each side's pattern read by one correlation").toBe(2)
    expect(counted.headers(), "each side's header read once per leg and once for the projection").toBeLessThanOrEqual(2 * (legs + 1))
  })

  it("does not cut a capture whose flows document does not project a header a required leg rule reads, naming Tools.emitHeaders", async () => {
    const unprojected = { ...linkedCallsFlows(1), emit_headers: [] }
    await expect(cutOf("unprojected.pcap", unprojected, neverDerives, requiring(LINK_RULE))).rejects.toThrow(
      `the leg rule "linked-call" reads X-Linked-Call: name X-Linked-Call in the deployment's Tools.emitHeaders`
    )
  })

  it("cuts a capture whose flows document does not project a header a skipped leg rule reads without it, naming it", async () => {
    const unprojected = { ...linkedCallsFlows(1), emit_headers: [] }
    const cut = await cutOf("skipped.pcap", unprojected, neverDerives, skipping(LINK_RULE))
    expect(cut.skipped).toEqual([{ rule: "linked-call", missing: ["X-Linked-Call"] }])
    expect(cut.cases, "the callee leg joined to no caller leg").toEqual([])
  })
})

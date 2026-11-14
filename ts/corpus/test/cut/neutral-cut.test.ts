/**
 * The neutral cut of one capture (`src/cut/neutral-cut.ts`), the base
 * generator's step, under an assembler whose Call-ID derivation counts how
 * often it is asked, or whose leg rule counts how often it is compiled: the
 * capture's legs are correlated once, whatever the number of calls it holds.
 */
import { NodeServices } from "@effect/platform-node"
import type { Flows, Rules } from "@sip/contracts"
import { CaseAssembler, LogicExtractor, Policy } from "@sip/pipeline"
import type { CallIdDerivation } from "@sip/pipeline/derivation"
import { neverDerives } from "@sip/pipeline/derivation"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { counting, manyCallsFlows, suffixDerives } from "../../../pipeline/test/fixtures.js"
import { countingPatterns, LINK_RULE, linkedCallsFlows } from "../../../pipeline/test/linked-legs.js"
import { cutCapture } from "../../src/cut/neutral-cut.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "neutral-cut-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The assembler bound to a policy joining legs by `derives` and `legRules` and stating nothing else. */
const assemblerJoining = (derives: CallIdDerivation, legRules: ReadonlyArray<Rules.HeaderPairRule>) =>
  CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(Policy.policyWith({ derives, legRules }))))

/** The neutral cut of the capture `capture` whose flows document is `flows`, its legs joined by `derives` and `legRules`. */
const cutOf = (capture: string, flows: Flows.FlowsDoc, derives: CallIdDerivation, legRules: ReadonlyArray<Rules.HeaderPairRule> = []) => {
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
 * reads its patterns again.
 */
describe("the neutral cut of a capture", () => {
  it.each([1, 3, 40])("asks the derivation about each derived leg once, over %i calls", async (calls) => {
    const counted = counting(suffixDerives)
    const cut = await cutOf(`calls-${calls}.pcap`, manyCallsFlows(calls), counted.derives)
    expect(cut.cases).toHaveLength(calls)
    expect(cut.refused).toEqual([])
    expect(counted.count()).toBe(calls)
  })

  it.each([1, 3, 40])("joins the legs a header-pair rule links in one correlation, over %i calls", async (calls) => {
    const counted = countingPatterns(LINK_RULE)
    const cut = await cutOf(`linked-${calls}.pcap`, linkedCallsFlows(calls), neverDerives, [counted.rule])
    expect(cut.cases).toHaveLength(calls)
    expect(cut.refused, "every callee leg joined to the caller leg it names").toEqual([])
    expect(counted.count(), "each side's pattern read by one correlation").toBe(2)
  })

  it("does not cut a capture whose flows document does not project a header the leg rule reads", async () => {
    const unprojected = { ...linkedCallsFlows(1), emit_headers: ["Call-ID"] }
    await expect(cutOf("unprojected.pcap", unprojected, neverDerives, [LINK_RULE])).rejects.toThrow("--emit-headers Call-ID,X-Linked-Call")
  })
})

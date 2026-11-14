/**
 * The neutral cut of one capture (`src/cut/neutral-cut.ts`), the base
 * generator's step, under an assembler whose Call-ID derivation counts how
 * often it is asked: the capture's legs are correlated once, whatever the
 * number of calls it holds.
 */
import { NodeServices } from "@effect/platform-node"
import { CaseAssembler, LogicExtractor, Policy } from "@sip/pipeline"
import type { CallIdDerivation } from "@sip/pipeline/derivation"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { counting, manyCallsFlows, suffixDerives } from "../../../pipeline/test/fixtures.js"
import { cutCapture } from "../../src/cut/neutral-cut.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "neutral-cut-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The assembler bound to a policy deriving by `derives` and stating nothing else. */
const assemblerDeriving = (derives: CallIdDerivation) =>
  CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(Policy.policyWith({ derives }))))

/** The neutral cut of a capture of `calls` answered calls, its legs joined by `derives`. */
const cutOf = (calls: number, derives: CallIdDerivation) => {
  const capture = `calls-${calls}.pcap`
  const srcDir = path.join(dir, capture, "src")
  fs.mkdirSync(srcDir, { recursive: true })
  fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify(manyCallsFlows(calls)))
  return Effect.runPromise(
    cutCapture({ capture, srcDir, outDir: path.join(dir, capture, "out") }).pipe(
      Effect.provide(Layer.mergeAll(NodeServices.layer, assemblerDeriving(derives)))
    )
  )
}

/**
 * Each call holds one derived leg, whose one suffix that is a Call-ID of the
 * document is its base: one correlation asks the derivation once per call,
 * and a second one anywhere in the cut asks every derived leg again.
 */
describe("the neutral cut of a capture", () => {
  it.each([1, 3, 40])("asks the derivation about each derived leg once, over %i calls", async (calls) => {
    const counted = counting(suffixDerives)
    const cut = await cutOf(calls, counted.derives)
    expect(cut.cases).toHaveLength(calls)
    expect(cut.refused).toEqual([])
    expect(counted.count()).toBe(calls)
  })
})

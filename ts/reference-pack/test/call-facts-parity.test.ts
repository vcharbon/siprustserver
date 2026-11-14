/**
 * Call facts on real captures: every capture the pack commits (`capture/`,
 * its vendored third-party ones included) is extracted by `sipflow`
 * (`./capture-cut.ts`), cut at the boundary of the system that took its
 * first INVITE, and each case's document, assembled by the real `assemble`,
 * is held against its capture on the subset the two sides of call facts agree
 * on (ADR-0061): who sent each leg's opening INVITE, its first 18x, its
 * outcome final, whether it was answered, how it ended and who ended it.
 *
 * The recorded captures are cut under the reference runner's counter-prefixed
 * Call-ID derivation, the vendored ones under none. The extractor is
 * `SIPFLOW_BIN`, else the release build of `sip-pcap`'s `sipflow`.
 */
import { Cut, DocumentFacts, FlowsFile, Parts, Plan, Policy, Profile, Sut } from "@sip/pipeline"
import { captureIndex } from "@sip/pipeline/capture-index"
import { decideCases } from "@sip/pipeline/caseset"
import { type CallIdDerivation, counterPrefixed, neverDerives } from "@sip/pipeline/derivation"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"
import { documentOf, ingest } from "./capture-cut.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CAPTURES = path.join(PACK, "capture")
const SIPFLOW = process.env["SIPFLOW_BIN"] ?? path.join(PACK, "../..", "target/release/sipflow")
const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
if (!skipping && !fs.existsSync(SIPFLOW)) {
  throw new Error(`no binary at ${SIPFLOW}: build it, name one with SIPFLOW_BIN, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1`)
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), "call-facts-parity-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

const NO_PLAN = new Plan.Plan({ default_cc: "", countries: [], max_composed_prefix: 0, fake_prefix: "" })
const ANY_ADDRESS = new Profile.SutProfile(["0.0.0.0/0"])

/**
 * What the cut makes of one committed capture: its cases, each with its
 * document's legs, its vantages, how many legs were held against the capture
 * and those that disagree.
 */
const parityOf = async (file: string, derives: CallIdDerivation) => {
  const capture = path.basename(file)
  const [done] = await ingest(SIPFLOW, path.join(work, "artifacts"), capture, file)
  expect([done?.capture, done?.broke], "ingested as it is").toEqual([capture, undefined])
  const flows = FlowsFile.readFlowsFile(documentOf(path.join(work, "artifacts"), capture))
  const { sut } = Sut.decideSut(flows, ANY_ADDRESS, capture)
  const index = captureIndex(flows, derives, NO_PLAN)
  let n = 0
  const cut = Cut.cutCalls(flows, sut, capture, index.correlation, () => `${capture}-${++n}`)
  const decided = decideCases({
    flows,
    capture,
    specs: cut.calls.map((call) => ({ id: call.id, uac: call.uac, uas: [...call.uas], defects: [], cutLegs: call.legs })),
    sut,
    plan: NO_PLAN,
    policy: Policy.policyWith({ derives }),
    index,
    parts: Parts.index(flows)
  })
  return decided.outcomes.flatMap((outcome) => {
    if (outcome.built === undefined) return []
    const vantages = [outcome.spec.uac, ...outcome.spec.uas]
    return [
      {
        legs: DocumentFacts.ofDocument(outcome.built.pivot).legs.size,
        vantages: vantages.length,
        ...DocumentFacts.parity(flows, sut, vantages, outcome.built.pivot)
      }
    ]
  })
}

describe.skipIf(skipping)("a document and the capture it was cut from", () => {
  it.each(["basic-call.pcapng", "emergency-call.pcapng", "reinvite-call.pcapng", "forked-reliable.pcapng"])(
    "agree on every leg of the recorded %s",
    async (name) => {
      const cases = await parityOf(path.join(CAPTURES, name), counterPrefixed)

      expect(cases.map((c) => [c.legs, c.vantages, c.compared]), "one case, each document leg held against its vantage").toEqual([
        [2, 2, 2]
      ])
      expect(cases.flatMap((c) => c.disagreements)).toEqual([])
    }
  )

  it("agree on every leg of the vendored blind transfer, the one call the capture holds whole", async () => {
    const cases = await parityOf(path.join(CAPTURES, "third-party", "refer-transfer.pcap"), neverDerives)

    expect(cases.map((c) => [c.legs, c.vantages, c.compared])).toEqual([[2, 2, 2]])
    expect(cases.flatMap((c) => c.disagreements)).toEqual([])
  })

  it("have nothing to agree on where the vendored capture yields no case", async () => {
    expect(await parityOf(path.join(CAPTURES, "third-party", "asterisk-22-xcid.pcap"), neverDerives)).toEqual([])
  })
})

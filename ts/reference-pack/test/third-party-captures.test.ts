/**
 * The vendored captures (`README.md`, "The vendored captures";
 * `capture/third-party/PROVENANCE.md`): each committed file still has the
 * SHA-256 its provenance states, and each is ingested with the identity
 * extraction and cut by the base's case generator under no Call-ID
 * derivation, as a capture of a stack the base knows nothing of is.
 *
 * - `refer-transfer.pcap`, through a record-routing proxy that keeps the
 *   Call-ID across its hop: the call whose transfer completes is cut into a
 *   case keeping its REFER, the 202 and both NOTIFYs (RFC 3515); the call the
 *   capture leaves open is refused as truncated and incomplete.
 * - `asterisk-22-xcid.pcap`, through a B2BUA giving the callee's leg a
 *   Call-ID of its own and naming the caller's in an `X-CID` header: the cut
 *   joins legs by Call-ID derivation alone, so no call is cut. Each callee
 *   leg is refused as missing its upstream leg, each caller leg as a dial the
 *   capture does not hold.
 *
 * The extractor is `SIPFLOW_BIN`, else the release build of `sip-pcap`'s
 * `sipflow`.
 */
import * as crypto from "node:crypto"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"
import { cut, ingest } from "./capture-cut.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const VENDORED = path.join(PACK, "capture", "third-party")

const SIPFLOW = process.env["SIPFLOW_BIN"] ?? path.join(CHECKOUT, "target/release/sipflow")
const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
if (!skipping && !fs.existsSync(SIPFLOW)) {
  throw new Error(`no binary at ${SIPFLOW}: build it, name one with SIPFLOW_BIN, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1`)
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), "third-party-captures-"))
const artifacts = path.join(work, "artifacts")

afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** Each vendored capture by its file name: the SHA-256 its source repository records of it. */
const HASHES: Readonly<Record<string, string>> = {
  "asterisk-22-xcid.pcap": "6e43618794c9e5d1b908831abe658c1b4f2494a886431edce9cf1ffc2a3b74cd",
  "refer-transfer.pcap": "d98fb2f4be3ce43dac3b6decb8b059c47f92114dfa3a3432e6618cd1e93cc6a2"
}

const sha256Of = (file: string): string => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex")

/** One refusal of the cut's `excluded.json`, as far as these checks read it. */
interface Refusal {
  readonly caseId: string
  readonly reason: string
  readonly callIds: ReadonlyArray<string>
}

/** What the cut made of `capture`: the cases it wrote and each refusal as `<case> <reason> <Call-IDs>`. */
const cutOf = async (capture: string) => {
  const [done] = await ingest(SIPFLOW, artifacts, capture, path.join(VENDORED, capture))
  expect([done?.capture, done?.broke], "ingested as it is").toEqual([capture, undefined])
  const out = path.join(work, capture, "cut")
  expect(cut(artifacts, capture, out, "none"), "cut, under no Call-ID derivation").toEqual([{ capture, cut: true }])
  const excluded = path.join(out, "excluded.json")
  const refusals = fs.existsSync(excluded) ? JSON.parse(fs.readFileSync(excluded, "utf8")) as ReadonlyArray<Refusal> : []
  return {
    out,
    cases: fs.readdirSync(path.join(out, "cases")),
    refused: refusals.map((refusal) => `${refusal.caseId} ${refusal.reason} ${refusal.callIds.join(",")}`).sort()
  }
}

/** One step of a cut document's flow, as far as these checks read it. */
interface Step {
  readonly leg: string
  readonly op: string
  readonly msg: { readonly method?: string; readonly status?: number; readonly "cseq-method"?: string }
}

/** Each message step of the case `id` cut into `out`: its leg, its op and its method or status with its CSeq method. */
const stepsOf = (out: string, id: string): ReadonlyArray<string> =>
  (JSON.parse(fs.readFileSync(path.join(out, "cases", id, `${id}.v3.json`), "utf8")) as { readonly flow: ReadonlyArray<Step> }).flow
    .map((step) => `${step.leg} ${step.op} ${step.msg.method ?? `${step.msg.status} ${step.msg["cseq-method"]}`}`)

describe("the vendored captures", () => {
  it("are committed as their source repository records them, each hash stated in the provenance", () => {
    const provenance = fs.readFileSync(path.join(VENDORED, "PROVENANCE.md"), "utf8")
    expect(fs.readdirSync(VENDORED).filter((file) => file.endsWith(".pcap")).sort()).toEqual(Object.keys(HASHES).sort())
    for (const [file, hash] of Object.entries(HASHES)) {
      expect(sha256Of(path.join(VENDORED, file)), file).toBe(hash)
      expect(provenance, `${file}'s hash in the provenance`).toContain(hash)
    }
  })

  describe.skipIf(skipping)("ingested and cut", () => {
    it("cuts the proxied call whose blind transfer completes into a case keeping the REFER and its NOTIFYs, and refuses the one left open", async () => {
      const { out, cases, refused } = await cutOf("refer-transfer.pcap")
      expect(cases).toEqual(["refer-transfer-1"])
      expect(stepsOf(out, "refer-transfer-1"), "the transfer between the caller's leg and the callee's").toEqual([
        "A send INVITE",
        "A expect 100 INVITE",
        "B expect INVITE",
        "B send 180 INVITE",
        "A expect 180 INVITE",
        "B send 200 INVITE",
        "A expect 200 INVITE",
        "A send ACK",
        "B expect ACK",
        "A send REFER",
        "B expect REFER",
        "B send 202 REFER",
        "A expect 202 REFER",
        "B send NOTIFY",
        "A expect NOTIFY",
        "A send 200 NOTIFY",
        "B expect 200 NOTIFY",
        "B send NOTIFY",
        "A expect NOTIFY",
        "A send 200 NOTIFY",
        "B expect 200 NOTIFY",
        "A send BYE",
        "B expect BYE",
        "B send 200 BYE",
        "A expect 200 BYE"
      ])
      expect(refused, "the call the capture leaves open, truncated and incomplete").toEqual([
        "refer-transfer-2 source-call-incomplete refer-pending-1@example.com",
        "refer-transfer-2 source-truncated-at-edge refer-pending-1@example.com"
      ])
    })

    it("cuts no call of the B2BUA whose legs only an X-CID header joins: each callee leg misses its upstream leg, each caller leg its dial", async () => {
      const { cases, refused } = await cutOf("asterisk-22-xcid.pcap")
      expect(cases).toEqual([])
      expect(refused).toEqual([
        "asterisk-22-xcid-1 source-dial-not-captured 1-xcid-call@example.com",
        "asterisk-22-xcid-2 missing-upstream-leg 207e9af6-c60c-4f7f-99ae-328d569004c6",
        "asterisk-22-xcid-3 source-dial-not-captured 2-xcid-call@example.com",
        "asterisk-22-xcid-4 missing-upstream-leg 6fdf4f81-40b1-4c21-ad36-e76cadd78c2d",
        "asterisk-22-xcid-5 source-dial-not-captured 3-xcid-call@example.com",
        "asterisk-22-xcid-6 missing-upstream-leg 801ebb0b-2ce6-4fb9-8868-36fb5319d0ec"
      ])
    })
  })
})

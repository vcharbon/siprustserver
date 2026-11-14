/**
 * The vendored captures (`README.md`, "The vendored captures";
 * `capture/third-party/PROVENANCE.md`): each committed file still has the
 * SHA-256 its provenance states, and each is ingested with the identity
 * extraction and cut by the base's case generator under no Call-ID derivation
 * and the base's default leg rules.
 *
 * - `refer-transfer.pcap`, through a record-routing proxy that keeps the
 *   Call-ID across its hop: the call whose transfer completes is cut into a
 *   case keeping its REFER, the 202 and both NOTIFYs (RFC 3515); the call the
 *   capture leaves open is refused as truncated and incomplete.
 * - `asterisk-22-xcid.pcap`, through a B2BUA giving the callee's leg a
 *   Call-ID of its own, the caller's named in an `X-CID` header a pre-dial
 *   handler of the source harness's dialplan adds: the default rule set's
 *   `X-CID` rule joins the two legs, so each of the three calls is cut into a
 *   case holding both.
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

/** Each row of the provenance's table: the file named in its first cell, the SHA-256 in its last. */
const rowsOf = (provenance: string): ReadonlyArray<readonly [string, string]> =>
  provenance.split("\n").flatMap((line) => {
    const row = /^\|\s*`([^`]+)`\s*\|.*\|\s*`([0-9a-f]{64})`\s*\|\s*$/.exec(line)
    return row === null ? [] : [[row[1]!, row[2]!] as const]
  })

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
    cases: fs.readdirSync(path.join(out, "cases")).sort(),
    refused: refusals.map((refusal) => `${refusal.caseId} ${refusal.reason} ${refusal.callIds.join(",")}`).sort()
  }
}

/** One step of a cut document's flow, as far as these checks read it. */
interface Step {
  readonly leg: string
  readonly op: string
  readonly msg: {
    readonly method?: string
    readonly status?: number
    readonly "cseq-method"?: string
    readonly headers?: ReadonlyArray<{ readonly name: string; readonly value: string }>
  }
}

/** A cut document, as far as these checks read it. */
interface Document {
  readonly case: { readonly source: { readonly call_groups: ReadonlyArray<number> } }
  readonly flow: ReadonlyArray<Step>
}

/** The document of the case `id` cut into `out`. */
const documentOf = (out: string, id: string): Document =>
  JSON.parse(fs.readFileSync(path.join(out, "cases", id, `${id}.v3.json`), "utf8")) as Document

/** Each message step of the case `id` cut into `out`: its leg, its op and its method or status with its CSeq method. */
const stepsOf = (out: string, id: string): ReadonlyArray<string> =>
  documentOf(out, id).flow
    .map((step) => `${step.leg} ${step.op} ${step.msg.method ?? `${step.msg.status} ${step.msg["cseq-method"]}`}`)

/** The call groups the case `id` cut into `out` was cut of, and each `X-CID` its callee leg's INVITE carries. */
const joinedOf = (out: string, id: string) => {
  const document = documentOf(out, id)
  return {
    groups: document.case.source.call_groups,
    xcid: document.flow
      .filter((step) => step.leg === "B" && step.msg.method === "INVITE")
      .flatMap((step) => (step.msg.headers ?? []).filter((h) => h.name === "X-CID").map((h) => h.value))
  }
}

describe("the vendored captures", () => {
  it("are committed as their source repository records them, each hash stated on its file's row of the provenance", () => {
    const provenance = fs.readFileSync(path.join(VENDORED, "PROVENANCE.md"), "utf8")
    expect(fs.readdirSync(VENDORED).filter((file) => file.endsWith(".pcap")).sort()).toEqual(Object.keys(HASHES).sort())
    for (const [file, hash] of Object.entries(HASHES)) {
      expect(sha256Of(path.join(VENDORED, file)), file).toBe(hash)
    }
    expect(Object.fromEntries(rowsOf(provenance)), "each file's row of the provenance, its hash").toEqual(HASHES)
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

    it("cuts each call of the B2BUA whose legs an X-CID header joins into a case holding the caller's leg and the callee's", async () => {
      const { out, cases, refused } = await cutOf("asterisk-22-xcid.pcap")
      expect(cases).toEqual(["asterisk-22-xcid-1", "asterisk-22-xcid-2", "asterisk-22-xcid-3"])
      expect(refused).toEqual([])
      expect(cases.map((id) => joinedOf(out, id)), "each case the caller's call group and the callee's, its INVITE naming that caller").toEqual([
        { groups: [0, 1], xcid: ["1-xcid-call@example.com"] },
        { groups: [2, 3], xcid: ["2-xcid-call@example.com"] },
        { groups: [4, 5], xcid: ["3-xcid-call@example.com"] }
      ])
      for (const id of cases) {
        expect(stepsOf(out, id), `${id}: the caller's leg A and the callee's leg B, the B2BUA ACKing the callee itself`).toEqual([
          "A send INVITE",
          "A expect 100 INVITE",
          "B expect INVITE",
          "B send 180 INVITE",
          "A expect 180 INVITE",
          "B send 200 INVITE",
          "B expect ACK",
          "A expect 200 INVITE",
          "A send ACK",
          "A send BYE",
          "A expect 200 BYE",
          "B expect BYE",
          "B send 200 BYE"
        ])
      }
    })
  })
})

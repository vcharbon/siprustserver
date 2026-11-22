/**
 * The reference replay (`README.md`, "The reference capture" and "The
 * recorded samples"): each capture the pack records is ingested with the
 * identity extraction (`@sip/corpus` `ExtractionIdentity`) by the extractor
 * `sipflow`, its kept flows document the raw one and its name kept when it
 * is extracted again; the base's case generator cuts it into one case, the
 * call between the caller and the callee side, which keeps what the call
 * shows: the forked call's two early dialogs on the callee's leg, each opened
 * by a provisional requiring `100rel` and PRACKed, the first answered; the
 * emergency call's `Resource-Priority` on the caller's INVITE; the held
 * call's re-INVITE on both legs; and the base's campaign run replays that
 * case on the simulated lane as its one cell.
 *
 * The extractor is `SIPFLOW_BIN`, else the release build of `sip-pcap`'s
 * `sipflow`; the replay and lint binaries are found as the reference
 * campaign's are (`reference-campaign.test.ts`). The suite builds and drops
 * its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { BaseCampaign, manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignRuns, Jobs } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { CALLER } from "../capture/answered-call.ts"
import { RESOURCE_PRIORITY } from "../capture/emergency-call.ts"
import { cut, documentOf, ingest } from "./capture-cut.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")

const binary = (variable: string, fallback: string): string => process.env[variable] ?? path.join(CHECKOUT, fallback)
const SIPFLOW = binary("SIPFLOW_BIN", "target/release/sipflow")
const REPLAY = binary("REPLAY_BIN", "target/debug/pivot-replay")
const PIVOT_SCHEMA = binary("PIVOT_SCHEMA_BIN", "target/release/pivot-schema")
const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
for (const [variable, file] of [["SIPFLOW_BIN", SIPFLOW], ["REPLAY_BIN", REPLAY], ["PIVOT_SCHEMA_BIN", PIVOT_SCHEMA]] as const) {
  if (!skipping && !fs.existsSync(file)) {
    throw new Error(`no binary at ${file}: build it, name one with ${variable}, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1`)
  }
}

const SCRATCH = await scratch("reference_replay_test")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "reference-replay-"))
const artifacts = path.join(work, "artifacts")

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
})

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
})

/** The committed capture `capture` ingested into `artifacts`. */
const ingested = (capture: string) => ingest(SIPFLOW, artifacts, capture, path.join(PACK, "capture", capture))

/** The base campaign run of the cut at `out` on the simulated lane: its exit code and the run the store holds of it. */
const replay = (out: string) =>
  Effect.gen(function*() {
    const manifest = yield* manifestOf(PACK)
    const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
      env: "reference-replay-test",
      checkout: CHECKOUT,
      pack: { dir: PACK, manifest }
    })
    return yield* Effect.gen(function*() {
      const job = yield* (yield* Jobs.Service).create({ kind: "campaign", args: {}, createdBy: "reference-replay-test" })
      const code = yield* BaseCampaign.run({
        pack: PACK,
        workspace: undefined,
        lanes: "simulated",
        out: path.join(out, "..", "run"),
        runs: work,
        junit: undefined,
        job,
        env: "reference-replay-test",
        site: "http://127.0.0.1:5175",
        replay: REPLAY,
        runner: "",
        pivotSchema: PIVOT_SCHEMA,
        collection: undefined,
        setup: [],
        sources: [out]
      })
      return { code, run: (yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })).at(0) }
    }).pipe(Effect.provide(services))
  }).pipe(Effect.provide(NodeServices.layer))

/** One step of a cut document's flow, as far as these checks read it. */
interface Step {
  readonly leg: string
  readonly op: string
  readonly early?: string
  readonly in_dialog?: boolean
  readonly msg: {
    readonly method?: string
    readonly status?: number
    readonly "cseq-method"?: string
    readonly headers?: ReadonlyArray<{ readonly name: string; readonly value: string }>
  }
}

/** The flow of the cut document at `file`. */
const flowIn = (file: string): ReadonlyArray<Step> => (JSON.parse(fs.readFileSync(file, "utf8")) as { readonly flow: ReadonlyArray<Step> }).flow

/**
 * Each early dialog of leg B in `flow`, in the order it opens: the
 * provisionals the callee side sends on it requiring `100rel` (RFC 3262), the
 * PRACKs it receives on it, and the INVITE's 2xx it sends on it.
 */
const forksOf = (flow: ReadonlyArray<Step>) => {
  const onB = flow.filter((step) => step.leg === "B" && step.early !== undefined)
  const requiresReliable = (step: Step) =>
    (step.msg.headers ?? []).some((header) => header.name.toLowerCase() === "require" && /\b100rel\b/.test(header.value))
  return [...new Set(onB.map((step) => step.early))].map((early) => {
    const steps = onB.filter((step) => step.early === early)
    const invite = (step: Step) => step.op === "send" && step.msg["cseq-method"] === "INVITE"
    return {
      reliable: steps.filter((step) => invite(step) && step.msg.status! < 200 && requiresReliable(step)).map((step) => step.msg.status),
      pracked: steps.filter((step) => step.op === "expect" && step.msg.method === "PRACK").length,
      answered: steps.filter((step) => invite(step) && step.msg.status! >= 200 && step.msg.status! < 300).length
    }
  })
}

/** The INVITE requests of `flow` on each leg, in order: who handles each, and whether it rides the confirmed dialog. */
const invitesOf = (flow: ReadonlyArray<Step>) =>
  flow.filter((step) => step.msg.method === "INVITE").map((step) => ({ leg: step.leg, op: step.op, in_dialog: step.in_dialog === true }))

/** The requests of `flow`, in order, each with its leg and its method. */
const requestsOf = (flow: ReadonlyArray<Step>): ReadonlyArray<string> =>
  flow.filter((step) => step.msg.method !== undefined).map((step) => `${step.leg} ${step.op} ${step.msg.method}`)

/** The requests of an answered call ended by the caller's BYE, as its cut states them. */
const ANSWERED = ["A send INVITE", "B expect INVITE", "A send ACK", "B expect ACK", "A send BYE", "B expect BYE"]

/** One recorded capture: its file, a party its kept document names, what its case keeps, and the check of that case's flow. */
interface Recorded {
  readonly capture: string
  readonly party: string
  readonly keeps: string
  readonly kept: (flow: ReadonlyArray<Step>) => void
}

const RECORDED: ReadonlyArray<Recorded> = [
  {
    capture: "forked-reliable.pcapng",
    party: "sip:alice@atlanta.example.com",
    keeps: "the forked call's case, both forks ringing reliably and PRACKed",
    kept: (flow) =>
      expect(forksOf(flow), "both forks kept, each ringing reliably and PRACKed").toEqual([
        { reliable: [183], pracked: 1, answered: 1 },
        { reliable: [180], pracked: 1, answered: 0 }
      ])
  },
  {
    capture: "basic-call.pcapng",
    party: CALLER.uri,
    keeps: "the basic call's case, answered and ended by BYE",
    kept: (flow) => expect(requestsOf(flow), "the answered call's requests on both legs").toEqual(ANSWERED)
  },
  {
    capture: "emergency-call.pcapng",
    party: CALLER.uri,
    keeps: "the emergency call's case, its INVITE's Resource-Priority kept",
    kept: (flow) => {
      expect(requestsOf(flow), "the answered call's requests on both legs").toEqual(ANSWERED)
      const invite = flow.find((step) => step.leg === "A" && step.op === "send" && step.msg.method === "INVITE")!
      expect(invite.msg.headers, "the caller's INVITE marked an emergency call").toContainEqual(expect.objectContaining(RESOURCE_PRIORITY))
    }
  },
  {
    capture: "reinvite-call.pcapng",
    party: CALLER.uri,
    keeps: "the held call's case, its re-INVITE on both legs",
    kept: (flow) =>
      expect(invitesOf(flow), "the dialing INVITE and the re-INVITE in the dialog, on both legs").toEqual([
        { leg: "A", op: "send", in_dialog: false },
        { leg: "B", op: "expect", in_dialog: false },
        { leg: "A", op: "send", in_dialog: true },
        { leg: "B", op: "expect", in_dialog: true }
      ])
  }
]

describe.skipIf(!served || skipping)("the reference replay", () => {
  for (const recorded of RECORDED) {
    const name = path.basename(recorded.capture, ".pcapng")
    it(`ingests ${recorded.capture} as it is, cuts it into ${recorded.keeps} and replays that case on the simulated lane`, async () => {
      const [first] = await ingested(recorded.capture)
      expect([first?.capture, first?.broke]).toEqual([recorded.capture, undefined])
      const document = documentOf(artifacts, recorded.capture)
      const kept = fs.readFileSync(document, "utf8")
      expect(kept, "the parties, as recorded").toContain(recorded.party)
      const [again] = await ingested(recorded.capture)
      expect([again?.capture, fs.readFileSync(document, "utf8") === kept], "extracted again under its name, the same bytes").toEqual([
        recorded.capture,
        true
      ])

      const out = path.join(work, name, "cut")
      expect(cut(artifacts, recorded.capture, out, "counter-prefix"), "the runner's legs joined by its Call-ID convention").toEqual([
        { capture: recorded.capture, cut: true }
      ])
      const cases = fs.readdirSync(path.join(out, "cases"))
      expect(cases, "the one call between the caller and the callee side").toEqual([`${name}-1`])
      recorded.kept(flowIn(path.join(out, "cases", cases[0]!, `${cases[0]!}.v3.json`)))

      const { code, run } = await Effect.runPromise(replay(out))
      const results = run!.lanes[0]!.results
      expect(results.map((result) => result.cell), "the cut's case replayed as the run's one cell").toEqual([`${name}-1`])
      expect(
        results.map((result) => [result.passed, result.verifications.filter((line) => line.answer === "fail").map((line) => `${line.name}: ${line.message}`)]),
        "the call replayed as recorded, every check holding"
      ).toEqual([[true, []]])
      expect(code).toBe(0)
    })
  }
})

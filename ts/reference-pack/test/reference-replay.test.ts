/**
 * The reference replay (`README.md`, "The reference capture"): the committed
 * capture of the forked call is ingested with the identity extraction
 * (`@sip/corpus` `ExtractionIdentity`) by the extractor `sipflow`, its kept
 * flows document the raw one and its name kept when it is extracted again;
 * the base's case generator cuts it into one case, the call between the
 * caller and the forking callee side, which keeps the fork: two early
 * dialogs on the callee's leg, each opened by a provisional requiring
 * `100rel` and PRACKed, the first answered; and the base's campaign run replays
 * that case on the simulated lane as its one cell.
 *
 * The extractor is `SIPFLOW_BIN`, else the release build of `sip-pcap`'s
 * `sipflow`; the replay and lint binaries are found as the reference
 * campaign's are (`reference-campaign.test.ts`). The suite builds and drops
 * its OWN database (`@sip/store/test-server`).
 */
import { NodeServices } from "@effect/platform-node"
import { ExtractionIdentity, Tools } from "@sip/corpus"
import { BASE } from "@sip/corpus/cut/generator"
import * as Extract from "@sip/corpus/ingest/extract"
import * as Record from "@sip/corpus/ingest/record"
import { BaseCampaign, manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignRuns, Jobs } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const CAPTURE = "forked-reliable.pcapng"
const COMMITTED = path.join(PACK, "capture", CAPTURE)

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
}, 120_000)

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
}, 120_000)

/** The extractor alone: the ingest reaches for no other tool. */
const tools = Tools.layerWith({
  ready: () => Effect.succeed(SIPFLOW),
  generator: Effect.succeed(BASE),
  measure: Effect.die("the ingest measures nothing"),
  compileMs: Effect.succeed(0),
  decides: Effect.die("the ingest decides nothing"),
  reading: []
})

/** The committed capture ingested into the store `artifacts` with the identity extraction. */
const ingest = () =>
  Effect.runPromise(
    Extract.phase([{ capture: CAPTURE, pcaps: [COMMITTED] }], artifacts, "sipflow", 1).pipe(
      Effect.provide(Layer.merge(tools, ExtractionIdentity.layer).pipe(Layer.provideMerge(NodeServices.layer)))
    )
  )

/**
 * The base generator run over the capture's directory into `out`, joining the
 * runner's legs by its own Call-ID convention: its manifest's lines.
 */
const cut = (out: string): ReadonlyArray<unknown> => {
  const list = path.join(work, "work-list.json")
  const manifest = path.join(work, "manifest.jsonl")
  fs.writeFileSync(list, JSON.stringify([{ capture: CAPTURE, srcDir: Record.captureDir(artifacts, CAPTURE), outDir: out }]))
  const [command, ...args] = BASE.argv
  execFileSync(command!, [...args, "--work-list", list, "--manifest", manifest, "--call-id-derivation", "counter-prefix"], {
    cwd: BASE.cwd,
    stdio: "pipe"
  })
  return fs.readFileSync(manifest, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as unknown)
}

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
        out: path.join(work, "run"),
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
      return { code, run: (yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 }))[0] }
    }).pipe(Effect.provide(services))
  }).pipe(Effect.provide(NodeServices.layer))

/** One step of a cut document's flow, as far as the fork shows in it. */
interface Step {
  readonly leg: string
  readonly op: string
  readonly early?: string
  readonly msg: {
    readonly method?: string
    readonly status?: number
    readonly "cseq-method"?: string
    readonly headers?: ReadonlyArray<{ readonly name: string; readonly value: string }>
  }
}

/**
 * Each early dialog of leg B in the cut document at `file`, in the order it
 * opens: the provisionals the callee side sends on it requiring `100rel`
 * (RFC 3262), the PRACKs it receives on it, and the INVITE's 2xx it sends on
 * it.
 */
const forksOf = (file: string) => {
  const flow = (JSON.parse(fs.readFileSync(file, "utf8")) as { readonly flow: ReadonlyArray<Step> }).flow
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

describe.skipIf(!served || skipping)("the reference replay", () => {
  it("ingests the capture as it is, cuts it into the forked call's case and replays that case on the simulated lane", async () => {
    const [ingested] = await ingest()
    expect([ingested?.capture, ingested?.broke]).toEqual([CAPTURE, undefined])
    const document = path.join(Record.captureDir(artifacts, CAPTURE), ExtractionIdentity.documentName(CAPTURE))
    const kept = fs.readFileSync(document, "utf8")
    expect(kept, "the caller's and the callee side's parties, as recorded").toContain("sip:alice@atlanta.example.com")
    const [again] = await ingest()
    expect([again?.capture, fs.readFileSync(document, "utf8") === kept], "extracted again under its name, the same bytes").toEqual([
      CAPTURE,
      true
    ])

    const out = path.join(work, "cut")
    expect(cut(out)).toEqual([{ capture: CAPTURE, cut: true }])
    const cases = fs.readdirSync(path.join(out, "cases"))
    expect(cases, "the one call between the caller and the forking callee side").toEqual(["forked-reliable-1"])
    expect(forksOf(path.join(out, "cases", cases[0]!, `${cases[0]!}.v3.json`)), "both forks kept, each ringing reliably and PRACKed").toEqual([
      { reliable: [183], pracked: 1, answered: 1 },
      { reliable: [180], pracked: 1, answered: 0 }
    ])

    const { code, run } = await Effect.runPromise(replay(out))
    const results = run!.lanes[0]!.results
    expect(results.map((result) => result.cell), "the cut's case replayed as the run's one cell").toEqual(["forked-reliable-1"])
    expect(
      results.map((result) => [result.passed, result.verifications.filter((line) => line.answer === "fail").map((line) => `${line.name}: ${line.message}`)]),
      "the forked call replayed as recorded, every check holding"
    ).toEqual([[true, []]])
    expect(code).toBe(0)
  })
})

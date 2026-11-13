/**
 * The reference capture's recording (`README.md`, "The reference capture"):
 * the base's campaign run plays the forked case (`capture/forked-reliable.ts`)
 * on the reference lane, through a workspace holding it on free loopback
 * addresses, and the B2BUA runner it stands captures the call. The cell
 * passes; the system relays each of the callee side's two reliable
 * provisionals to the caller on an early dialog of its own, each PRACKed on
 * both legs, and answers the caller once, from the fork that answered. With
 * `REFERENCE_CAPTURE_WRITE=1` the run's capture replaces the committed one,
 * `capture/forked-reliable.pcapng` (`just reference-capture`).
 *
 * A case source leading out of the pack is refused before the run's job
 * opens. The suite builds and drops its OWN database
 * (`@sip/store/test-server`); its binaries are found as the reference
 * campaign's are (`reference-campaign.test.ts`).
 */
import { NodeServices } from "@effect/platform-node"
import { BaseCampaign, manifestOf } from "@sip/siprig/base-campaign"
import { layerOf } from "@sip/siprig/base-campaign-services"
import { CampaignResults, CampaignRuns, JobEvents, Jobs } from "@sip/store"
import { layerFromUrl } from "@sip/store/client"
import { drop, fresh, migrated, on, scratch, served, urlFor } from "@sip/store/test-server"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { TITLE } from "../capture/forked-reliable.ts"
import { blocksOf, payloadOf } from "../collectors/capture/cut.ts"
import { hostedWorkspace, writtenUnder } from "./hosted-workspace.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const SOURCE = "./capture/forked-reliable.ts"
const COMMITTED = path.join(PACK, "capture", "forked-reliable.pcapng")

const binary = (variable: string, fallback: string): string => process.env[variable] ?? path.join(CHECKOUT, fallback)
const REPLAY = binary("REPLAY_BIN", "target/debug/pivot-replay")
const RUNNER = binary("B2BUA_RUNNER_BIN", "target/debug/b2bua-runner")
const PIVOT_SCHEMA = binary("PIVOT_SCHEMA_BIN", "target/release/pivot-schema")
const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"

const SCRATCH = await scratch("reference_capture_test")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "reference-capture-"))

beforeAll(async () => {
  if (!served) return
  await fresh(SCRATCH)
  await on(SCRATCH, migrated)
}, 120_000)

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
}, 120_000)

/** The base campaign run of `sources` on `lane`: its exit code, the run the store holds of it and its job's events. */
const recorded = (lane: string, workspace: string | undefined, sources: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const manifest = yield* manifestOf(PACK)
    const services = layerOf(layerFromUrl(urlFor(SCRATCH)), path.join(work, "sessions"), {
      env: "reference-capture-test",
      checkout: CHECKOUT,
      pack: { dir: PACK, manifest }
    })
    return yield* Effect.gen(function*() {
      const job = yield* (yield* Jobs.Service).create({ kind: "campaign", args: {}, createdBy: "reference-capture-test" })
      const code = yield* BaseCampaign.run({
        pack: PACK,
        workspace,
        lanes: lane,
        out: path.join(work, `run-${lane}-${job}`),
        runs: work,
        junit: undefined,
        job,
        env: "reference-capture-test",
        site: "http://127.0.0.1:5175",
        replay: REPLAY,
        runner: RUNNER,
        pivotSchema: PIVOT_SCHEMA,
        collection: undefined,
        setup: [],
        sources
      })
      const runs = yield* (yield* CampaignRuns.Service).read("reference", { limit: 1 })
      const events = yield* (yield* JobEvents.Service).ofJob(job)
      return { code, run: code === 2 ? undefined : runs[0], events }
    }).pipe(Effect.provide(services))
  }).pipe(Effect.provide(NodeServices.layer))

/** One SIP message of a capture: its start line and the headers this suite reads. */
interface Message {
  readonly start: string
  readonly toTag: string | undefined
  readonly require: string | undefined
  readonly callId: string
  readonly cseq: string
}

const headerOf = (lines: ReadonlyArray<string>, name: string): string | undefined =>
  lines.find((line) => line.toLowerCase().startsWith(`${name.toLowerCase()}:`))?.slice(name.length + 1).trim()

/** The SIP messages of a pcapng capture, in order, once each: a sent datagram is also received on loopback. */
const messagesIn = (capture: Uint8Array): ReadonlyArray<Message> => {
  const seen = new Set<string>()
  const messages: Array<Message> = []
  for (const block of blocksOf(capture).filter((one) => one.type === 6)) {
    const text = new TextDecoder("latin1").decode(payloadOf(block))
    if (seen.has(text)) continue
    seen.add(text)
    const lines = text.split("\r\n\r\n", 1)[0]!.split("\r\n")
    messages.push({
      start: lines[0]!,
      toTag: /;tag=([^;>\s]+)/.exec(headerOf(lines, "To") ?? "")?.[1],
      require: headerOf(lines, "Require"),
      callId: headerOf(lines, "Call-ID") ?? "",
      cseq: headerOf(lines, "CSeq") ?? ""
    })
  }
  return messages
}

describe.skipIf(!served || skipping)("the reference capture", () => {
  it("records through the runner a call whose callee side forks, each fork ringing reliably on its own early dialog, one answering", async () => {
    fs.mkdirSync(path.join(work, "recording"), { recursive: true })
    const written = writtenUnder(path.join(work, "recording"), "b2bua-runner")
    const workspace = await hostedWorkspace(PACK, path.join(work, "workspace"), written)
    const { code, run } = await Effect.runPromise(recorded("reference", workspace, [SOURCE]))
    expect(code).toBe(0)
    expect(run!.lanes[0]!.results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed])).toEqual([
      [TITLE, true]
    ])
    const capture = fs.readFileSync(written.capture)
    const messages = messagesIn(capture)
    const legs = [...new Set(messages.map((message) => message.callId))]
    expect(legs, "the caller's leg and the callee side's").toHaveLength(2)
    for (const leg of legs) {
      const on = messages.filter((message) => message.callId === leg)
      const provisionals = on.filter((message) => /^SIP\/2\.0 18[03] /.test(message.start))
      expect(provisionals.map((message) => message.require), "each provisional reliable").toEqual(["100rel", "100rel"])
      const tags = provisionals.map((message) => message.toTag)
      expect(new Set(tags).size, "two early dialogs").toBe(2)
      const pracks = on.filter((message) => message.start.startsWith("PRACK "))
      expect(pracks.map((message) => message.toTag), "each PRACKed on its own early dialog").toEqual(tags)
      const finals = on.filter((message) => /^SIP\/2\.0 [2-6]\d\d /.test(message.start) && message.cseq.endsWith(" INVITE"))
      expect(finals.map((message) => [message.start, message.toTag]), "one final, the first fork's answer").toEqual([
        ["SIP/2.0 200 OK", tags[0]]
      ])
    }
    if (process.env["REFERENCE_CAPTURE_WRITE"] === "1") fs.copyFileSync(written.capture, COMMITTED)
  })

  it("refuses, before its job opens, a case source leading out of the pack", async () => {
    const { code, events } = await Effect.runPromise(recorded("simulated", undefined, ["../basic-case/test/fixtures/cases.ts"]))
    expect(code).toBe(2)
    expect(events).toEqual([])
  })
})

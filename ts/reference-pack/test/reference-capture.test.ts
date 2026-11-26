/**
 * The recording of the pack's captures (`README.md`, "The reference capture"
 * and "The recorded samples"): for each recorded case, the base's campaign
 * run plays its source on the reference lane, through a workspace holding it
 * on free loopback addresses, and the B2BUA runner it stands captures the
 * call. Each cell passes and its capture shows the call on both legs: the
 * forked call's two reliable provisionals each relayed on an early dialog of
 * its own, PRACKed on both legs, and answered once, from the fork that
 * answered; the basic call answered and ended by BYE, with no
 * `Resource-Priority`; the emergency call's `Resource-Priority` carried to
 * the callee side; the re-INVITE's hold offer and its answer on both legs.
 * Each answered call shows the parties' display names and numbers on the
 * caller's leg. With `REFERENCE_CAPTURE_WRITE=1` each run's capture replaces
 * its committed one under `capture/` (`just reference-capture`). Each
 * committed capture shows the same, with no database or binary at hand.
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
import { CALLEE, CALLER } from "../capture/answered-call.ts"
import { TITLE as BASIC } from "../capture/basic-call.ts"
import { RESOURCE_PRIORITY, TITLE as EMERGENCY } from "../capture/emergency-call.ts"
import { TITLE as FORKED } from "../capture/forked-reliable.ts"
import { TITLE as REINVITE } from "../capture/reinvite-call.ts"
import { blocksOf, payloadOf } from "../collectors/capture/cut.ts"
import { hostedWorkspace, writtenUnder } from "./hosted-workspace.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")

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
})

afterAll(async () => {
  fs.rmSync(work, { recursive: true, force: true })
  if (served) await drop(SCRATCH)
})

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

/**
 * The parties of the workspace's reference lane bound on a loopback address
 * of their own, apart from the runner's: a cut tells the system under test
 * from its peers by address.
 */
const partiesApart = (workspace: string): void => {
  const lane = path.join(workspace, "lanes", "reference.toml")
  fs.writeFileSync(lane, fs.readFileSync(lane, "utf8").replace(`endpoint_host = "127.0.0.1"`, `endpoint_host = "${PARTIES}"`))
}

/** The loopback address the recording's parties are bound on. */
const PARTIES = "127.0.0.2"

/** One SIP message of a capture: its start line, its header lines and its body. */
interface Message {
  readonly start: string
  readonly headers: ReadonlyArray<string>
  readonly body: string
}

/** The value of the first header `name` of `message`, where it has one. */
const headerOf = (message: Message, name: string): string | undefined =>
  message.headers.find((line) => line.toLowerCase().startsWith(`${name.toLowerCase()}:`))?.slice(name.length + 1).trim()

const toTagOf = (message: Message): string | undefined => /;tag=([^;>\s]+)/.exec(headerOf(message, "To") ?? "")?.[1]

/** The SIP messages of a pcapng capture, in order, once each: a sent datagram is also received on loopback. */
const messagesIn = (capture: Uint8Array): ReadonlyArray<Message> => {
  const seen = new Set<string>()
  const messages: Array<Message> = []
  for (const block of blocksOf(capture).filter((one) => one.type === 6)) {
    const text = new TextDecoder("latin1").decode(payloadOf(block))
    if (seen.has(text)) continue
    seen.add(text)
    const [head, body] = text.split("\r\n\r\n", 2) as [string, string | undefined]
    const [start, ...headers] = head.split("\r\n")
    messages.push({ start: start!, headers, body: body ?? "" })
  }
  return messages
}

/** The legs of a capture, the caller's first: each its messages, in order. */
const legsOf = (messages: ReadonlyArray<Message>): ReadonlyArray<ReadonlyArray<Message>> =>
  [...new Set(messages.map((message) => headerOf(message, "Call-ID") ?? ""))].map((leg) =>
    messages.filter((message) => headerOf(message, "Call-ID") === leg)
  )

/** Each message of a leg as its method or status and its CSeq: `INVITE 1 INVITE`, `200 1 INVITE`. */
const ladderOf = (leg: ReadonlyArray<Message>): ReadonlyArray<string> =>
  leg.map((message) => `${/^SIP\/2\.0 (\d{3})/.exec(message.start)?.[1] ?? message.start.split(" ")[0]} ${headerOf(message, "CSeq")}`)

/** The INVITEs of a leg, in order. */
const invitesOf = (leg: ReadonlyArray<Message>): ReadonlyArray<Message> => leg.filter((message) => message.start.startsWith("INVITE "))

/** The forked call on each leg: two early dialogs, each opened reliably and PRACKed, the first answered. */
const forked = (legs: ReadonlyArray<ReadonlyArray<Message>>): void => {
  for (const on of legs) {
    const provisionals = on.filter((message) => /^SIP\/2\.0 18[03] /.test(message.start))
    expect(provisionals.map((message) => headerOf(message, "Require")), "each provisional reliable").toEqual(["100rel", "100rel"])
    const tags = provisionals.map(toTagOf)
    expect(new Set(tags).size, "two early dialogs").toBe(2)
    const pracks = on.filter((message) => message.start.startsWith("PRACK "))
    expect(pracks.map(toTagOf), "each PRACKed on its own early dialog").toEqual(tags)
    const finals = on.filter((message) => /^SIP\/2\.0 [2-6]\d\d /.test(message.start) && (headerOf(message, "CSeq") ?? "").endsWith(" INVITE"))
    expect(finals.map((message) => [message.start, toTagOf(message)]), "one final, the first fork's answer").toEqual([
      ["SIP/2.0 200 OK", tags[0]]
    ])
  }
}

/** The caller's leg of an answered call ended by its BYE, as the ladder shows it. */
const CALLER_LADDER = ["INVITE 1 INVITE", "100 1 INVITE", "180 1 INVITE", "200 1 INVITE", "ACK 1 ACK", "BYE 2 BYE", "200 2 BYE"]

/** The callee side's leg of the same call. */
const CALLEE_LADDER = ["INVITE 1 INVITE", "180 1 INVITE", "200 1 INVITE", "ACK 1 ACK", "BYE 2 BYE", "200 2 BYE"]

/** The name-addr of a From or To, its tag dropped. */
const addressOf = (message: Message, name: "From" | "To"): string => (headerOf(message, name) ?? "").replace(/;tag=[^;]*$/, "")

/**
 * The answered call on each leg, and the parties' identities its INVITEs
 * carry: each party's display name and number on the caller's leg, each
 * number on the callee side's, where the system writes no display name.
 */
const answered = (legs: ReadonlyArray<ReadonlyArray<Message>>): void => {
  expect(legs.map(ladderOf), "the caller's leg and the callee side's, answered, ended by BYE").toEqual([CALLER_LADDER, CALLEE_LADDER])
  expect(legs.map((leg) => [addressOf(invitesOf(leg)[0]!, "From"), addressOf(invitesOf(leg)[0]!, "To")]), "the parties on each leg").toEqual([
    [`"${CALLER.display}" <${CALLER.uri}>`, `"${CALLEE.display}" <${CALLEE.uri}>`],
    [`<${CALLER.uri}>`, `<${CALLEE.uri}>`]
  ])
}

/** The basic call: answered, no message on either leg carrying a `Resource-Priority`. */
const basic = (legs: ReadonlyArray<ReadonlyArray<Message>>): void => {
  answered(legs)
  expect(legs.flatMap((leg) => leg.flatMap((message) => headerOf(message, RESOURCE_PRIORITY.name) ?? [])), "no priority mark on either leg")
    .toEqual([])
}

/** The emergency call: answered, its INVITE's `Resource-Priority` on both legs. */
const emergency = (legs: ReadonlyArray<ReadonlyArray<Message>>): void => {
  answered(legs)
  expect(legs.map((leg) => headerOf(invitesOf(leg)[0]!, RESOURCE_PRIORITY.name)), "the emergency mark on both legs").toEqual([
    RESOURCE_PRIORITY.value,
    RESOURCE_PRIORITY.value
  ])
}

/** The held call on each leg: the re-INVITE's sendonly offer and its recvonly answer, CSeq 2, before the BYE. */
const held = (legs: ReadonlyArray<ReadonlyArray<Message>>): void => {
  const ended = ["BYE 3 BYE", "200 3 BYE"]
  expect(legs.map(ladderOf), "the caller's leg and the callee side's, answered, held, ended by BYE").toEqual([
    [...CALLER_LADDER.slice(0, -2), "INVITE 2 INVITE", "100 2 INVITE", "200 2 INVITE", "ACK 2 ACK", ...ended],
    [...CALLEE_LADDER.slice(0, -2), "INVITE 2 INVITE", "200 2 INVITE", "ACK 2 ACK", ...ended]
  ])
  for (const leg of legs) {
    const reinvite = invitesOf(leg)[1]!
    const answer = leg.find((message) => message.start.startsWith("SIP/2.0 200 ") && headerOf(message, "CSeq") === "2 INVITE")!
    expect([reinvite.body, answer.body].map((body) => body.split("\r\n").filter((line) => /^(a=(send|recv)only|o=)/.test(line))), "the hold offer and its answer").toEqual([
      ["o=- 1 2 IN IP4 192.0.2.10", "a=sendonly"],
      ["o=- 1 2 IN IP4 192.0.2.20", "a=recvonly"]
    ])
  }
}

/** One recorded case: its source, the capture committed of it, its title, and what its capture shows. */
interface Recording {
  readonly source: string
  readonly capture: string
  readonly title: string
  readonly shows: string
  readonly wire: (legs: ReadonlyArray<ReadonlyArray<Message>>) => void
}

const RECORDINGS: ReadonlyArray<Recording> = [
  {
    source: "./capture/forked-reliable.ts",
    capture: "forked-reliable.pcapng",
    title: FORKED,
    shows: "a call whose callee side forks, each fork ringing reliably on its own early dialog, one answering",
    wire: forked
  },
  {
    source: "./capture/basic-call.ts",
    capture: "basic-call.pcapng",
    title: BASIC,
    shows: "a call answered and ended by the caller's BYE",
    wire: basic
  },
  {
    source: "./capture/emergency-call.ts",
    capture: "emergency-call.pcapng",
    title: EMERGENCY,
    shows: "an emergency call, its Resource-Priority carried to the callee side",
    wire: emergency
  },
  {
    source: "./capture/reinvite-call.ts",
    capture: "reinvite-call.pcapng",
    title: REINVITE,
    shows: "a call held by the caller's re-INVITE, then ended by BYE",
    wire: held
  }
]

/** Checks that `capture` holds the caller's leg and the callee side's, each showing what `recording` records. */
const shows = (recording: Recording, capture: Uint8Array): void => {
  const legs = legsOf(messagesIn(capture))
  expect(legs, "the caller's leg and the callee side's").toHaveLength(2)
  recording.wire(legs)
}

describe("the committed captures", () => {
  for (const recording of RECORDINGS) {
    it(`show in ${recording.capture} ${recording.shows}`, () => {
      shows(recording, fs.readFileSync(path.join(PACK, "capture", recording.capture)))
    })
  }
})

describe.skipIf(!served || skipping)("the reference capture", () => {
  for (const recording of RECORDINGS) {
    it(`records through the runner ${recording.shows}`, async () => {
      const dir = path.join(work, path.basename(recording.capture, ".pcapng"))
      fs.mkdirSync(path.join(dir, "recording"), { recursive: true })
      const written = writtenUnder(path.join(dir, "recording"), "b2bua-runner")
      const workspace = await hostedWorkspace(PACK, path.join(dir, "workspace"), written)
      partiesApart(workspace)
      const { code, run } = await Effect.runPromise(recorded("reference", workspace, [recording.source]))
      expect(code).toBe(0)
      expect(run!.lanes[0]!.results.map((result) => [CampaignResults.caseNameOf(result.caseId).ref, result.passed])).toEqual([
        [recording.title, true]
      ])
      shows(recording, fs.readFileSync(written.capture))
      if (process.env["REFERENCE_CAPTURE_WRITE"] === "1") fs.copyFileSync(written.capture, path.join(PACK, "capture", recording.capture))
    })
  }

  it("refuses, before its job opens, a case source leading out of the pack", async () => {
    const { code, events } = await Effect.runPromise(recorded("simulated", undefined, ["../basic-case/test/fixtures/cases.ts"]))
    expect(code).toBe(2)
    expect(events).toEqual([])
  })
})

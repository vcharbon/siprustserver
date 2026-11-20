/**
 * Each route family of the site over the fixture pack and a store answering
 * only what a test states: what a route answers is read off the pack's lanes,
 * commands, id forms, campaigns, roster and site text, and off the store's
 * rows, never a deployment's own.
 */
import type { Attachments, Jobs, Model } from "@sip/store"
import { Effect } from "effect"
import { HttpBody, type HttpClient } from "effect/unstable/http"
import { ConnectionError, SqlError } from "effect/unstable/sql/SqlError"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { PACK, values } from "./fixture-pack.ts"
import { ask, get } from "./server.ts"

const dirs: Array<string> = []
afterAll(() => {
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true })
})

/** A fresh directory, removed once the file's tests end. */
const scratch = (): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "site-routes-"))
  dirs.push(dir)
  return dir
}

/** `body` posted as JSON to `url`. */
const postJson = (url: string, body: unknown) => (client: HttpClient.HttpClient) =>
  client.post(url, { body: HttpBody.jsonUnsafe(body) })

describe("the catalog routes", () => {
  it("list the pack's lanes in its order, a simulated lane never exclusive", async () => {
    const answer = await get(values(), {}, "/api/lanes")
    expect(answer.status).toBe(200)
    const { lanes } = answer.json as { lanes: ReadonlyArray<{ lane: string; exclusive: boolean }> }
    expect(lanes.map((one) => one.lane)).toEqual(["sim", "hosted", "dark"])
    expect(lanes[0]!.exclusive).toBe(false)
  })

  it("state the site's name in its config, the rig's then the pack's title, as /llms.txt names it", async () => {
    const answer = await get(values(), {}, "/api/config")
    expect(answer.status).toBe(200)
    const { name } = answer.json as { name: string }
    expect(name).toBe("siprig · Fixture Pack")
    const map = await get(values(), { campaignRuns: { captures: Effect.succeed([]) } }, "/llms.txt")
    expect(map.body.split("\n")[0]).toBe(`# ${name}`)
  })
})

describe("the triage routes", () => {
  it("refuse a capture nothing holds, naming the pack's capture id form", async () => {
    const answer = await get(
      values(),
      { runs: { standing: () => Effect.succeed(null) }, cuts: { standing: () => Effect.succeed(null) } },
      "/api/triage/nothing-here.pcap"
    )
    expect(answer.status).toBe(404)
    expect(answer.json).toMatchObject({ id: "nothing-here.pcap", accepted: PACK.triage.forms.capture })
  })

  it("state the line a capture's failed extraction left, as the deployment's extraction reads it", async () => {
    const corpus = scratch()
    fs.mkdirSync(path.join(corpus, "broken.pcap"))
    fs.writeFileSync(path.join(corpus, "broken.pcap", "extraction-failure.txt"), "sipflow failed: truncated record")
    fs.mkdirSync(path.join(corpus, "whole.pcap"))
    const nothingStanding = { runs: { standing: () => Effect.succeed(null) }, cuts: { standing: () => Effect.succeed(null) } }
    const study = async (capture: string) => {
      const answer = await get(values({ corpus }), nothingStanding, `/api/triage/${capture}`)
      expect(answer.status).toBe(200)
      return (answer.json as { extractionFailure?: string }).extractionFailure
    }
    expect(await study("broken.pcap")).toBe("sipflow failed: truncated record")
    expect(await study("whole.pcap")).toBeUndefined()
  })
})

describe("the delta sheet", () => {
  it("reads the queue of the lane asked, and says why where the store does not answer", async () => {
    const asked: Array<string> = []
    const answer = await get(
      values(),
      {
        calls: {
          queue: (lane) =>
            Effect.sync(() => asked.push(lane)).pipe(
              Effect.andThen(Effect.fail(new SqlError({ reason: new ConnectionError({ cause: new Error("gone"), message: "store down" }) })))
            )
        }
      },
      "/api/deltas?lane=hosted"
    )
    expect(answer.status).toBe(200)
    expect(asked).toEqual(["hosted"])
    expect(answer.json).toMatchObject({ available: false, lane: "hosted" })
    expect((answer.json as { detail: string }).detail).toContain("store down")
  })
})

describe("the capture drop", () => {
  it("runs the source command over the saved file, its launch then read as a job of the pack's job command", async () => {
    const dir = scratch()
    const config = values({
      uploads: path.join(dir, "uploads"),
      ingest: { command: "sh", args: ["-c", "echo 'job 9: ingest launched'", "source"], cwd: dir }
    })
    const form = new FormData()
    form.append("file", new Blob([new Uint8Array([0xd4, 0xc3, 0xb2, 0xa1])]), "sample.pcap")
    const answer = await ask(config, {}, (client) =>
      client.post("/api/captures", { body: HttpBody.formData(form) }).pipe(
        Effect.andThen(client.get("/api/captures/sample/ingest"))
      ))
    expect(answer.status).toBe(200)
    expect(fs.existsSync(path.join(dir, "uploads", "sample.pcap"))).toBe(true)
    expect(answer.json).toMatchObject({ last: { exit: 0 }, detail: "launched as a job: `rig job list`" })
  })
})

describe("the rules page", () => {
  it("lists the pack's roster and names the pack's command where no standing run carries per-rule counts", async () => {
    const answer = await get(
      values(),
      {
        runs: { lanes: Effect.succeed([]) },
        ruleHits: {
          standingTotals: () => Effect.succeed([]),
          standingCoverage: () => Effect.succeed({ runs: 2, measured: 0 })
        },
        declared: { declared: Effect.succeed([]), population: Effect.succeed({ captures: 0, cases: 0 }) }
      },
      "/api/rules"
    )
    expect(answer.status).toBe(200)
    const view = answer.json as { detail: string; rows: ReadonlyArray<{ id: string; standing: string }> }
    expect(view.detail).toBe("no standing run carries per-rule counts — run `rig rule-hits`")
    expect(view.rows).toMatchObject([{ id: "late-ack", standing: "unexercised" }])
  })
})

describe("the request builder", () => {
  it("refuses a preview while the cut store holds no cut, naming the pack's cut command", async () => {
    const answer = await ask(
      values(),
      { cuts: { standingPrep: Effect.succeed(null) } },
      postJson("/api/requests/preview", { lanes: ["hosted"], selection: { kind: "captures", captures: ["a.pcap"] } })
    )
    expect(answer.status).toBe(400)
    expect(answer.json).toMatchObject({ detail: "the cut store holds no cut — run `rig cut` first" })
  })
})

/** Job `id`, running on `hosted` under no process the host can see. */
const runningJob = (id: number): Jobs.Row => ({
  id,
  kind: "replay",
  requestId: null,
  lane: "hosted",
  args: {},
  status: "running",
  createdAt: "2026-09-20T10:00:00.000Z",
  createdBy: "test",
  pid: null,
  startedAt: "2026-09-20T10:00:01.000Z",
  finishedAt: null,
  exit: null,
  logPath: null,
  waitingOn: null,
  sweepId: null,
  batchAt: null,
  batches: null,
  stopRequestedAt: null,
  stopGrace: null,
  restartOf: null,
  planned: null
})

const PROGRESS: Jobs.Progress = {
  total: 1,
  landed: 0,
  pending: 1,
  goneRefused: 0,
  goneMissing: 0,
  passed: 0,
  failed: 0,
  skipped: 0,
  runs: 0,
  openRuns: 0
}

describe("the job console", () => {
  it("refuses to stop a running job whose process is gone, naming the pack's job command that closes it", async () => {
    const answer = await ask(
      values(),
      { jobs: { show: (id) => Effect.succeed(id === 3 ? { job: runningJob(3), progress: PROGRESS } : null) } },
      postJson("/api/jobs/3/stop", {})
    )
    expect(answer.status).toBe(400)
    expect(answer.json).toMatchObject({ detail: "job 3's process is gone — `rig job reap` closes it" })
  })

  it("answers a job the store holds no row of as missing", async () => {
    const answer = await get(values(), { jobs: { show: () => Effect.succeed(null) } }, "/api/jobs/4")
    expect(answer.status).toBe(404)
    expect(answer.json).toMatchObject({ id: "job 4" })
  })
})

describe("the source list", () => {
  it("asks the store for the page the query states, beside the roots a source may lie under", async () => {
    const asked: Array<unknown> = []
    const answer = await get(
      values(),
      {
        sources: {
          list: (query) =>
            Effect.sync(() => {
              asked.push(query)
              return { rows: [], total: 0 }
            })
        }
      },
      "/api/sources?stale=1&q=sample&limit=5&offset=10"
    )
    expect(answer.status).toBe(200)
    expect(asked).toMatchObject([{ stale: true, q: "sample", limit: 5, offset: 10 }])
    expect((answer.json as { roots: ReadonlyArray<string> }).roots).toContain("/nowhere/pcaps")
  })
})

describe("the pcaps page", () => {
  it("states how a capture replay fits each of the pack's lanes: none where a lane has no routing path", async () => {
    const answer = await get(values(), {}, "/api/pcaps")
    expect(answer.status).toBe(200)
    const { lanes } = answer.json as { lanes: ReadonlyArray<{ lane: string; fit: string }> }
    expect(lanes.map((one) => one.lane)).toEqual(["sim", "hosted", "dark"])
    expect(lanes.find((one) => one.lane === "dark")!.fit).toBe("none")
    expect(lanes.find((one) => one.lane === "hosted")!.fit).not.toBe("none")
  })
})

describe("the reports", () => {
  it("refuse a compare that names one side as the caller's error, saying which side is missing", async () => {
    const answer = await get(values(), {}, "/api/compare?a=run1")
    expect(answer.status).toBe(400)
    expect(answer.json).toMatchObject({ reason: "name both sides: a=<selector>&b=<selector> (b is missing)" })
  })
})

describe("the attachment routes", () => {
  it("list a job's attachments off the store, and refuse a query naming two holders with the forms it takes", async () => {
    const asked: Array<Attachments.Holder> = []
    const members = {
      attachments: {
        of: (holder: Attachments.Holder) => Effect.sync(() => (asked.push(holder), []))
      }
    }
    const listed = await get(values(), members, "/api/attachments?job=7")
    expect(listed.status).toBe(200)
    expect(asked).toEqual([{ scope: "job", jobId: 7 }])
    const refused = await get(values(), members, "/api/attachments?job=7&batch=3")
    expect(refused.status).toBe(404)
    expect((refused.json as { accepted: string }).accepted).toContain("batch=<id>")
    expect(asked).toHaveLength(1)
  })
})

describe("the campaign list", () => {
  it("lists the pack's defined campaigns by title and the stored ones whose bundles are here, sorted", async () => {
    const sessions = scratch()
    fs.mkdirSync(path.join(sessions, "kept"))
    const answer = await get(
      values({ sessions }),
      { campaignRuns: { captures: Effect.succeed(["kept", "elsewhere"]) } },
      "/api/campaign",
      sessions
    )
    expect(answer.status).toBe(200)
    expect(answer.json).toEqual([{ capture: "kept", title: null }, { capture: "smoke", title: "Smoke run" }])
  })
})

/** The standing run of `cap.pcap` on `hosted`: two calls, one failed. */
const STANDING: Model.Standing = {
  capture: "cap.pcap",
  lane: "hosted",
  runId: 5,
  status: "done",
  prepId: 1,
  bundlePath: "/nowhere/run5",
  finishedAt: "2026-10-27T08:42:57.000Z",
  cases: 2,
  failed: 1,
  skipped: 0,
  caseIds: [],
  failedSteps: [],
  skipReasons: [],
  caseResults: [
    { caseId: "cap.pcap-k1", passed: false, failedStep: "s4", skipped: null },
    { caseId: "cap.pcap-k2", passed: true, failedStep: null, skipped: null }
  ],
  rfcGating: 0,
  deltas: { cases: 0, accepted: 0, knownBug: 0, unlisted: 0, unknown: 0, compared: 0, unreferenced: 0, records: 0 },
  lastOkRunId: null,
  lastOkBundlePath: null,
  caseRuns: []
}

describe("the page overviews", () => {
  it("route a capture's study to its failed calls, per lane of the pack holding a standing run", async () => {
    const answer = await ask(
      values(),
      { runs: { standing: (capture, lane) => Effect.succeed(capture === "cap.pcap" && lane === "hosted" ? STANDING : null) } },
      (client) =>
        client.get(`/api/links?url=${encodeURIComponent("/triage/cap.pcap")}`, { headers: { accept: "application/json" } })
    )
    expect(answer.status).toBe(200)
    const overview = answer.json as { sections: ReadonlyArray<{ heading: string; links: ReadonlyArray<{ label: string }> }> }
    const lanes = overview.sections.filter((one) => one.heading !== "Site")
    expect(lanes.map((one) => one.heading)).toEqual(["hosted · run 5: 1 of 2 calls failed"])
    expect(lanes[0]!.links.map((one) => one.label)).toEqual(["cap.pcap-k1 (failed at s4)"])
  })
})

describe("the site map", () => {
  it("names the site by the rig and the pack's title, and each lane with its SUT as the pack states it", async () => {
    const answer = await get(values(), { campaignRuns: { captures: Effect.succeed([]) } }, "/llms.txt")
    expect(answer.status).toBe(200)
    expect(answer.body.split("\n")[0]).toBe("# siprig · Fixture Pack")
    expect(answer.body).toContain(`- sim: ${PACK.simulatedSut}`)
    expect(answer.body).toContain("- hosted: a B2BUA on a test bench")
  })
})

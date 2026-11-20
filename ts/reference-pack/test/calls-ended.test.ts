/**
 * The `calls-ended` collector's answer for one cell, against an operator
 * server standing in for the runner's (`GET /admin/calls`): `pass` where the
 * runner lists none of the cell's Call-IDs, whatever other calls it serves;
 * `fail` where it lists one of them, naming it; the step refused where the
 * runner cannot list its calls.
 */
import { spawn } from "node:child_process"
import * as fs from "node:fs"
import * as http from "node:http"
import type * as net from "node:net"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"

const MAIN = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../collectors/calls-ended/main.ts")
const work = fs.mkdtempSync(path.join(os.tmpdir(), "calls-ended-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** The legs' Call-IDs of the cell the collector answers: the caller's and the runner's b-leg. */
const CELL = ["a84b4c76e66710@pc33.atlanta.example.com", "1-a84b4c76e66710@pc33.atlanta.example.com"]

/** Another call the runner serves at the same time. */
const OTHER = "3848276298220188511@atlanta.example.com"

/** An operator server answering `GET /admin/calls` with `status` and `calls`. */
const operator = async (status: number, calls: ReadonlyArray<ReadonlyArray<string>>) => {
  const server = http.createServer((request, response) => {
    if (request.url !== "/admin/calls") return response.writeHead(404).end()
    response.writeHead(status, { "content-type": "application/json" })
    response.end(JSON.stringify({ calls: calls.map((ids, n) => ({ call_ref: `ref-${n}`, call_ids: ids })) }))
  })
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve))
  return { server, admin: `127.0.0.1:${(server.address() as net.AddressInfo).port}` }
}

/** The collector's `collect` for the cell, against the operator at `admin`, in the directory `name`: its exit, its lines and its reading. */
const collected = async (name: string, admin: string) => {
  const dir = path.join(work, name)
  const out = path.join(dir, "out")
  fs.mkdirSync(out, { recursive: true })
  const context = path.join(dir, "context.json")
  fs.writeFileSync(
    context,
    JSON.stringify({
      cells: [{ id: "cell", legs: CELL.map((call_id) => ({ call_id })) }],
      lane: { parts: { "b2bua-runner": { value: { admin } } } }
    })
  )
  const child = spawn("node", [MAIN], {
    env: { ...process.env, SIPRIG_STEP: "collect", SIPRIG_CONTEXT: context, SIPRIG_OUT: out },
    stdio: ["ignore", "pipe", "pipe"]
  })
  const output = { stdout: "", stderr: "" }
  child.stdout.on("data", (chunk: Buffer) => void (output.stdout += chunk.toString()))
  child.stderr.on("data", (chunk: Buffer) => void (output.stderr += chunk.toString()))
  const status = await new Promise<number | null>((resolve) => child.once("close", resolve))
  const lines = output.stdout.split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as Record<string, unknown>)
  const reading = path.join(out, "reading.json")
  return {
    status,
    stderr: output.stderr,
    answer: lines.find((line) => line["verification"] === "calls-ended"),
    reading: fs.existsSync(reading) ? JSON.parse(fs.readFileSync(reading, "utf8")) as Record<string, unknown> : undefined
  }
}

describe("calls-ended", () => {
  it("passes a cell none of whose calls the runner lists, whatever else it serves", async () => {
    const { server, admin } = await operator(200, [[OTHER, `1-${OTHER}`]])
    try {
      const ran = await collected("other", admin)
      expect(ran.status).toBe(0)
      expect([ran.answer?.["answer"], ran.reading?.["held"]]).toEqual(["pass", []])
    } finally {
      server.close()
    }
  })

  it("fails a cell one of whose calls the runner still lists, naming its Call-ID", async () => {
    const { server, admin } = await operator(200, [[OTHER], [CELL[0]!, CELL[1]!]])
    try {
      const ran = await collected("held", admin)
      expect(ran.status).toBe(0)
      expect(ran.answer?.["answer"]).toBe("fail")
      expect(String(ran.answer?.["message"])).toContain(CELL[0]!)
      expect(ran.reading?.["held"]).toEqual(CELL)
    } finally {
      server.close()
    }
  })

  it("refuses the step where the runner cannot list its calls", async () => {
    const { server, admin } = await operator(503, [])
    try {
      const ran = await collected("unlisted", admin)
      expect(ran.status).toBe(1)
      expect(ran.answer).toBeUndefined()
      expect(ran.stderr).toContain("503")
    } finally {
      server.close()
    }
  })
})

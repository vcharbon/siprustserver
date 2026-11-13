/**
 * The reference pack's `restart-worker` on the reference lane, as a job's
 * setup runs it (`@sip/rig` `Setup`), around what a batch of the job does
 * there: stand the base's runner on the lane's addresses. A runner an earlier
 * job left on those addresses, named in the file of the lane's `runner-pid`
 * part, is stopped by the inject, so the base stands the job's own runner
 * there, naming it in the same pid file while it runs; the heal and its probe
 * find the stopped one gone. A pid file naming a process the lane's admin
 * address does not vouch for refuses the inject, the process left running:
 * no runner answers there, the address answers without a runner's state, or
 * the runner answering is another process (a stale pid file). A lane binding
 * the operation while it names no `runner-pid` part is refused at load.
 *
 * The runner is `B2BUA_RUNNER_BIN`, else the build of `b2bua-runner`; a
 * missing one FAILS, naming its variable; `TOOLCHAIN_TEST_SKIP=1` skips the
 * suite instead.
 */
import { NodeServices } from "@effect/platform-node"
import { LaneCatalog, OperationRun, Setup } from "@sip/rig"
import { runnerEnvOf, runnerPartOf, standSut } from "@sip/siprig/base-hosted-lane"
import { readingOf } from "@sip/siprig/base-lane-hosting"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import { type ChildProcess, spawn } from "node:child_process"
import * as fs from "node:fs"
import * as http from "node:http"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"
import { hostedWorkspace, writtenUnder } from "./hosted-workspace.ts"

const PACK = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const CHECKOUT = path.resolve(PACK, "../..")
const RUNNER = process.env["B2BUA_RUNNER_BIN"] ?? path.join(CHECKOUT, "target/debug/b2bua-runner")

const skipping = process.env["TOOLCHAIN_TEST_SKIP"] === "1"
if (!skipping && !fs.existsSync(RUNNER)) {
  throw new Error(`no binary at ${RUNNER}: build it, name one with B2BUA_RUNNER_BIN, or skip the binary-bound suites with TOOLCHAIN_TEST_SKIP=1`)
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), "restart-worker-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** The reference lane of a fresh workspace under `name`, the rig path it loads with, and its runner's pid file. */
const referenceLane = async (name: string) => {
  const dir = path.join(work, name)
  const written = writtenUnder(dir, "b2bua-runner")
  const workspace = await hostedWorkspace(PACK, dir, written)
  const catalog = await Effect.runPromise(
    LaneCatalog.load(readingOf(PACK), LaneCatalog.dirsUnder(workspace)).pipe(Effect.provide(NodeServices.layer))
  )
  return { dir, catalog, lane: LaneCatalog.hosted(catalog, "reference")!, pid: written.pid }
}

/** How `child` ended, once it has. */
const endOf = (child: ChildProcess) =>
  new Promise<{ readonly code: number | null; readonly signal: NodeJS.Signals | null }>((resolve) =>
    child.once("exit", (code, signal) => resolve({ code, signal }))
  )

/** Resolves once the runner `child` printed its ready line; rejects where it `ended` first. */
const readyOf = (child: ChildProcess, ended: ReturnType<typeof endOf>) =>
  Promise.race([
    new Promise<void>((resolve) => child.stdout!.on("data", (chunk: Buffer) => chunk.toString().includes("listening UDP") && resolve())),
    ended.then((end) => Promise.reject(new Error(`the runner exited before its ready line: ${JSON.stringify(end)}`)))
  ])

/** Whether the process `pid` runs. */
const running = (pid: number): boolean => {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/** The admin address of the lane's `b2bua-runner` part. */
const adminOf = (found: Awaited<ReturnType<typeof referenceLane>>): string => Result.getOrThrow(runnerPartOf(found.lane)).admin

/** `address` (`host:port`) as its host and port. */
const splitAddress = (address: string): readonly [string, number] => {
  const at = address.lastIndexOf(":")
  return [address.slice(0, at), Number(address.slice(at + 1))]
}

/** What the setup's inject step wrote to its log. */
const injectLogOf = (found: Awaited<ReturnType<typeof referenceLane>>): string => {
  const root = path.join(found.dir, "operations")
  const [instance] = fs.readdirSync(root)
  return fs.readFileSync(path.join(root, instance!, "inject.log"), "utf8")
}

/** The setup `restart-worker` on `lane` run around `body`, its events in order. */
const restarted = <A, E>(found: Awaited<ReturnType<typeof referenceLane>>, body: Effect.Effect<A, E, never>) => {
  const planned = Setup.planOf(found.catalog.rig, "the test's setup", ["restart-worker"], [found.lane.loaded], path.join(found.dir, "operations"))
  if (Result.isFailure(planned)) throw new Error(planned.failure.message)
  const events: Array<OperationRun.Event> = []
  const sink: OperationRun.Sink = (event) => Effect.sync(() => void events.push(event))
  return Effect.runPromise(Effect.result(Setup.around(planned.success, sink, body))).then((ended) => ({ ended, events }))
}

describe.skipIf(skipping)("restart-worker on the reference lane", () => {
  it("stops the runner an earlier job left on the lane's addresses, so the job's own runner stands there", async () => {
    const found = await referenceLane("left")
    const part = Result.getOrThrow(runnerPartOf(found.lane))
    const left = spawn(RUNNER, [], { env: { ...process.env, ...runnerEnvOf(found.lane, part) }, stdio: ["ignore", "pipe", "pipe"] })
    try {
      const ended = endOf(left)
      await readyOf(left, ended)
      fs.writeFileSync(found.pid, `${left.pid}\n`)
      let named: number | undefined
      const stood = Effect.gen(function*() {
        const sut = yield* standSut(RUNNER, found.lane, path.join(found.dir, "batch"))
        named = Number(fs.readFileSync(found.pid, "utf8").trim())
        yield* sut!.stop
      }).pipe(Effect.provide(NodeServices.layer))
      const { ended: setup, events } = await restarted(found, stood)
      expect(Result.isSuccess(setup), String(Result.isFailure(setup) ? setup.failure : "")).toBe(true)
      expect(await ended, "the runner left on the lane stopped by its SIGTERM, its drain done").toEqual({ code: 0, signal: null })
      expect(named, "the job's own runner named in the pid file while it ran").not.toBe(left.pid)
      expect(events.map((event) => event.phase)).toEqual(["inject", "injected", "heal", "healed", "recovered"])
      expect(fs.existsSync(found.pid), "no runner named once the job's stopped").toBe(false)
    } finally {
      left.kill("SIGKILL")
    }
  })

  it("refuses at load a lane binding it whose runner names its pid in no file", async () => {
    const dir = path.join(work, "unnamed")
    await hostedWorkspace(PACK, dir, writtenUnder(dir, "b2bua-runner"))
    const lane = path.join(dir, "lanes", "reference.toml")
    fs.writeFileSync(lane, fs.readFileSync(lane, "utf8").replace(/^runner-pid = .*$/m, ""))
    const loaded = await Effect.runPromise(
      Effect.result(LaneCatalog.load(readingOf(PACK), LaneCatalog.dirsUnder(dir))).pipe(Effect.provide(NodeServices.layer))
    )
    expect(Result.isFailure(loaded) ? JSON.stringify(loaded.failure) : "loaded").toContain(
      "operations.restart-worker: restart-worker needs the part runner-pid, which the lane names none of"
    )
  })

  it("refuses to signal a process its pid file names where no runner answers at the lane's admin address", async () => {
    const found = await referenceLane("stranger")
    const stranger = spawn("sleep", ["30"], { stdio: "ignore" })
    fs.writeFileSync(found.pid, `${stranger.pid}\n`)
    try {
      const { ended, events } = await restarted(found, Effect.void)
      expect(Result.isFailure(ended) ? String(ended.failure) : "", "the inject's refusal").toContain("inject")
      expect(injectLogOf(found)).toContain(`but no runner answers at ${adminOf(found)}: not signalled`)
      expect(running(stranger.pid!), "the process the pid file names, left running").toBe(true)
      expect(events.map((event) => event.phase)).toEqual(["inject", "failed", "heal", "healed", "recovered"])
    } finally {
      stranger.kill("SIGKILL")
    }
  })

  it("refuses to signal a process its pid file names where the admin address answers 200 with no runner's state", async () => {
    const found = await referenceLane("stateless")
    const [host, port] = splitAddress(adminOf(found))
    const server = http.createServer((_request, response) => response.writeHead(200, { "content-type": "application/json" }).end("{}"))
    await new Promise<void>((resolve) => server.listen(port, host, resolve))
    const stranger = spawn("sleep", ["30"], { stdio: "ignore" })
    fs.writeFileSync(found.pid, `${stranger.pid}\n`)
    try {
      const { ended } = await restarted(found, Effect.void)
      expect(Result.isFailure(ended), "the inject refused").toBe(true)
      expect(injectLogOf(found)).toContain(`but no runner answers at ${adminOf(found)}: not signalled`)
      expect(running(stranger.pid!), "the process the pid file names, left running").toBe(true)
    } finally {
      stranger.kill("SIGKILL")
      server.close()
    }
  })

  it("refuses to signal a process its pid file names where the runner answering at the lane's admin address is another process", async () => {
    const found = await referenceLane("reused")
    const part = Result.getOrThrow(runnerPartOf(found.lane))
    const runner = spawn(RUNNER, [], { env: { ...process.env, ...runnerEnvOf(found.lane, part) }, stdio: ["ignore", "pipe", "pipe"] })
    const stranger = spawn("sleep", ["30"], { stdio: "ignore" })
    try {
      await readyOf(runner, endOf(runner))
      fs.writeFileSync(found.pid, `${stranger.pid}\n`)
      const { ended } = await restarted(found, Effect.void)
      expect(Result.isFailure(ended), "the inject refused").toBe(true)
      expect(injectLogOf(found)).toContain(
        `the pid file names process ${stranger.pid}, but the runner at ${adminOf(found)} is process ${runner.pid}: not signalled`
      )
      expect(running(stranger.pid!), "the process the stale pid file names, left running").toBe(true)
      expect(running(runner.pid!), "the runner the pid file does not name, left running").toBe(true)
    } finally {
      stranger.kill("SIGKILL")
      runner.kill("SIGKILL")
    }
  })
})

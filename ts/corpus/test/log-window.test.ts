/**
 * A deployment's log cut by wall-clock window, off the file by bisection:
 * both stamp heads read, an unstamped line kept with the line above it, both
 * bounds included, whatever the window and wherever a probe lands.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as LogWindow from "../src/log-window.js"

const at = (iso: string) => Date.parse(iso)

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "log-window-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

const LINES = [
  "exit 0",
  "sut  | 2026-10-14T09:00:00.100000000Z boot",
  "sut  | 2026-10-14T09:00:01.250000000Z INVITE callId=\"A-1@pivot.invalid\" | GB: 1",
  "sut  |   stack traceback: continued",
  "sut  | 2026-10-14T09:00:02.000000000Z BYE callId=\"A-1@pivot.invalid\"",
  "sut  | 2026-10-14T09:00:05.000000000Z INVITE callId=\"A-2@pivot.invalid\""
]
const LOG = path.join(dir, "sut.log")
fs.writeFileSync(LOG, `${LINES.join("\n")}\n`)

const cut = (from: string, to: string) =>
  Effect.runPromise(LogWindow.cutFile(LOG, { fromMs: at(from), toMs: at(to) }).pipe(Effect.provide(NodeServices.layer)))

describe("LogWindow", () => {
  it("reads the instant at the head of a compose line and of a docker line, nanoseconds and all", () => {
    expect(LogWindow.stampOf("sut  | 2026-10-14T09:00:01.250000000Z x")).toBe(at("2026-10-14T09:00:01.250Z"))
    expect(LogWindow.stampOf("2026-10-14T09:00:01.250Z x | y")).toBe(at("2026-10-14T09:00:01.250Z"))
    expect(LogWindow.stampOf("sut  |   stack traceback")).toBeUndefined()
    expect(LogWindow.stampOf("exit 0")).toBeUndefined()
  })

  it("cuts the lines inside a window, an unstamped line with the line above it", async () => {
    expect(await cut("2026-10-14T09:00:01.250Z", "2026-10-14T09:00:02Z")).toBe(`${LINES.slice(2, 5).join("\n")}\n`)
    expect(await cut("2026-10-14T09:00:03Z", "2026-10-14T09:00:04Z")).toBe("")
    expect(await cut("2026-10-14T08:00:00Z", "2026-10-14T10:00:00Z")).toBe(`${LINES.slice(1).join("\n")}\n`)
    expect(await cut("2026-10-14T09:00:05Z", "2026-10-14T09:00:05Z")).toBe(`${LINES[5]}\n`)
  })

  it("cuts a long log by bisection exactly as a walk over every line would", async () => {
    const long = path.join(dir, "long.log")
    const base = at("2026-10-14T09:00:00Z")
    const lines = ["exit 0"]
    for (let i = 0; i < 20_000; i++) {
      lines.push(`sut  | ${new Date(base + i * 10).toISOString()} line ${i} ${"x".repeat(i % 97)}`)
      if (i % 7 === 0) lines.push(`sut  |   continued ${i}`)
    }
    fs.writeFileSync(long, `${lines.join("\n")}\n`)
    const walk = (fromMs: number, toMs: number): string => {
      let stamp = -Infinity
      return lines.filter((line) => {
        stamp = LogWindow.stampOf(line) ?? stamp
        return stamp >= fromMs && stamp <= toMs
      }).map((line) => `${line}\n`).join("")
    }
    for (const [from, to] of [[0, 5], [1234, 5678], [199_990, 300_000], [-50, 0], [50_005, 50_005]] as const) {
      const window = { fromMs: base + from, toMs: base + to }
      expect(await Effect.runPromise(LogWindow.cutFile(long, window).pipe(Effect.provide(NodeServices.layer)))).toBe(walk(window.fromMs, window.toMs))
    }
  })

  it("pads a window by a second on each side", () => {
    expect(LogWindow.padded({ fromMs: 10_000, toMs: 20_000 })).toEqual({ fromMs: 9_000, toMs: 21_000 })
  })
})

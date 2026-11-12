/**
 * Sample collectors for the collector runner's tests: real executables that
 * append a line to `trace` at the root of their rig path for each step they
 * run, so a test reads which steps ran and in what order. Each name states its
 * behaviour. Also a cell description and an in-memory locker to run them with.
 */
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Cell from "../src/cell.js"
import type * as CollectorRun from "../src/collector-run.js"
import type { Entry } from "./tree.js"

const sh = (body: string): Entry => ({ text: `#!/bin/sh\n${body}\n`, executable: true })

const node = (body: string): Entry => ({ text: `#!/usr/bin/env node\nconst fs = require("node:fs")\n${body}\n`, executable: true })

/** A step that appends `<line>` to the trace two levels above the collector's directory, then runs `then`. */
const traced = (line: string, then = "exit 0"): Entry => sh(`echo "${line}" >> ../../trace\n${then}`)

/** The files of collector `name` of `scope`: its manifest (`extra` lines before `[run]`) and the steps given. */
const collector = (
  name: string,
  steps: { readonly open?: Entry; readonly collect?: Entry; readonly close?: Entry },
  extra = "",
  scope = "batch"
): Record<string, Entry> => {
  const named = (["open", "collect", "close"] as const).filter((step) => step === "collect" || steps[step] !== undefined)
  return {
    [`collectors/${name}/collector.toml`]: `schema = "collector.v1"\nname = "${name}"\nscope = "${scope}"\n${extra}\n[run]\n${
      named.map((step) => `${step} = "./${step}"\n`).join("")
    }`,
    ...Object.fromEntries(named.map((step) => [`collectors/${name}/${step}`, steps[step] ?? traced(`${step} ${name}`)]))
  }
}

/**
 * `tidy`'s collect: per cell, a passing `tidy` line, a log attached and a key
 * added, all on its stdout; a failing line of the scope's own; and, in
 * `SIPRIG_OUT`'s `lines.jsonl`, a record assigned to the first cell.
 */
const TIDY_COLLECT = node(`
const ctx = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))
const token = fs.readFileSync(process.env.SIPRIG_STATE + "/token", "utf8").trim()
fs.appendFileSync("../../trace", "collect tidy " + token + " " + process.env.SIPRIG_STEP + "\\n")
fs.copyFileSync(process.env.SIPRIG_CONTEXT, "../../collect-context.json")
for (const cell of ctx.cells) {
  fs.writeFileSync(process.env.SIPRIG_OUT + "/" + cell.id + ".log", "log of " + cell.id + "\\n")
  console.log(JSON.stringify({ verification: "tidy", cell: cell.id, answer: "pass" }))
  console.log(JSON.stringify({ attachment: { kind: "log", title: "its log", file: cell.id + ".log" }, cell: cell.id }))
  console.log(JSON.stringify({ key: "trunk", cell: cell.id, values: ["t1"] }))
}
console.log(JSON.stringify({ verification: "tidy", answer: "fail", message: "the scope's own" }))
fs.writeFileSync(process.env.SIPRIG_OUT + "/lines.jsonl", JSON.stringify({ record: { n: 1 }, cell: ctx.cells[0].id }) + "\\n")
`)

/** A collect that prints `line` on its stdout. */
const answering = (line: string): Entry => sh(`echo '${line}'`)

/** Every sample collector, as one rig path's files. */
export const SAMPLES: Readonly<Record<string, Entry>> = {
  ...collector("tidy", {
    open: traced(
      "open tidy $SIPRIG_STEP",
      `cp "$SIPRIG_CONTEXT" ../../open-context.json\nls -A "$SIPRIG_STATE" > ../../state-seen\necho token > "$SIPRIG_STATE/token"`
    ),
    collect: TIDY_COLLECT,
    close: traced("close tidy $(cat $SIPRIG_STATE/token) $SIPRIG_STEP")
  }, `answers = ["tidy"]\nneeds = ["records-store"]\n[params]\npoll_s = 1`),
  ...collector("failing-collect", {
    close: traced("close failing-collect"),
    collect: traced("collect failing-collect", `echo "the records store is down" >&2\nexit 3`)
  }, `answers = ["records", "count"]`),
  ...collector("informative-failing", {
    collect: traced("collect informative-failing", `echo "the log store is down" >&2\nexit 3`)
  }, `informative = true`),
  ...collector("informative-noting", {
    collect: answering(`{"verification": "note", "cell": "c1", "answer": "fail", "message": "a slow log"}`)
  }, `informative = true\nanswers = ["note"]`),
  ...collector("failing-open", {
    open: traced("open failing-open", `echo "no route to the store" >&2\nexit 2`),
    collect: traced("collect failing-open"),
    close: traced("close failing-open")
  }),
  ...collector("hanging-collect", {
    collect: traced("collect hanging-collect", `sleep 30 &\necho $! > ../../sleeper\nwait`)
  }, `[timeouts]\ncollect_s = 0.5`),
  ...collector("leaves-process", {
    open: traced("open leaves-process", `sleep 30 > /dev/null 2>&1 &\necho $! > "$SIPRIG_STATE/pid"\necho $! > ../../sleeper`),
    close: traced("close leaves-process", `kill "$(cat "$SIPRIG_STATE/pid")"`)
  }),
  ...collector("slow-open", { open: traced("open slow-open", "sleep 5"), close: traced("close slow-open") }),
  ...collector("failing-close", { close: traced("close failing-close", "exit 1") }),
  ...collector("flaky-close", {
    close: traced("close flaky-close", `[ -e "$SIPRIG_STATE/tried" ] && exit 0\ntouch "$SIPRIG_STATE/tried"\nexit 1`)
  }),
  ...collector("locked", { open: traced("open locked"), close: traced("close locked") }, `locks = ["records-chain"]`),
  ...collector("also-locked", { open: traced("open also-locked"), close: traced("close also-locked") }, `locks = ["records-chain", "store"]`),
  ...collector("not-json", { collect: answering("the store said hello") }),
  ...collector("stranger", { collect: answering(`{"verification": "someone-else", "answer": "pass"}`) }),
  ...collector("wrong-cell", { collect: answering(`{"verification": "wrong-cell", "cell": "c9", "answer": "pass"}`) }),
  ...collector("lost-file", { collect: answering(`{"attachment": {"kind": "log", "file": "gone.log"}}`) }),
  ...collector("job-wide", {}, `needs = ["records-store"]`, "job"),
  ...collector("job-plain", {}, "", "job"),
  ...collector("refusing-check", {
    collect: sh(`echo "collect refusing-check" >> ../../trace\necho '{"verification": "target-up", "answer": "fail", "message": "nothing takes SIP on 192.0.2.1:5060: the target is down"}'`),
    close: traced("close refusing-check")
  }, `at = "before"\nanswers = ["target-up"]`),
  ...collector("unsure-check", {
    collect: traced("collect unsure-check", `echo "the lab cannot be asked" >&2\nexit 3`)
  }, `at = "before"`),
  ...collector("required-blind", {
    collect: traced(
      "collect required-blind",
      `echo '{"verification": "lab-reachable", "answer": "not-checkable", "message": "the lab cannot be asked"}'`
    )
  }, `at = "before"\nrequired = true\nanswers = ["lab-reachable"]`),
  ...collector("required-failing", {
    collect: traced("collect required-failing", `echo "the lab cannot be asked" >&2\nexit 3`)
  }, `at = "before"\nrequired = true`),
  ...collector("required-hanging", {
    collect: traced("collect required-hanging", `sleep 30 &\necho $! > ../../sleeper\nwait`)
  }, `at = "before"\nrequired = true\n[timeouts]\ncollect_s = 0.5`),
  ...collector("required-check", {}, `at = "before"\nrequired = true\nneeds = ["records-store"]`, "job"),
  ...collector("asked-check", {
    collect: traced("collect asked-check", `echo "the lab cannot be asked" >&2\nexit 3`)
  }, `at = "before"\ndefault = "on-demand"`, "job"),
  ...collector("on-failure", {
    open: traced("open on-failure"),
    collect: node(`
const ctx = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))
fs.appendFileSync("../../trace", "collect on-failure " + ctx.cells.map((cell) => cell.id).join(",") + "\\n")
for (const cell of ctx.cells) console.log(JSON.stringify({ verification: "on-failure", cell: cell.id, answer: "pass" }))
`),
    close: traced("close on-failure")
  }, `default = "on-failure"`),
  ...collector("after-on-failure", {}, `after = ["on-failure"]`),
  ...collector("on-failure-promising", {}, `default = "on-failure"\nanswers = ["kept-trace"]`),
  ...collector("fails-c2", { collect: answering(`{"verification": "fails-c2", "cell": "c2", "answer": "fail"}`) }),
  ...collector("on-demand", {
    collect: node(`
const ctx = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))
fs.appendFileSync("../../trace", "collect on-demand " + ctx.cells.map((cell) => cell.id).join(",") + "\\n")
fs.appendFileSync("../../trace", "window " + ctx.window.start.wall_ms + "-" + ctx.window.end.wall_ms + "\\n")
for (const cell of ctx.cells) {
  fs.writeFileSync(process.env.SIPRIG_OUT + "/" + cell.id + ".log", "asked of " + cell.id + "\\n")
  console.log(JSON.stringify({ attachment: { kind: "log", title: "asked now", file: cell.id + ".log" }, cell: cell.id }))
}
`)
  }, `default = "on-demand"\nneeds = ["records-store"]`),
  ...collector("pre-check", {}, `at = "before"`)
}

/** The trace lines under the rig path `root`, in the order they were written. */
export const traceOf = (root: string): ReadonlyArray<string> => {
  const file = path.join(root, "trace")
  return fs.existsSync(file) ? fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "") : []
}

/** A cell `id` of lane `lane` whose directory is `dir`, as a collector reads it. */
export const cellOf = (id: string, lane: string, dir: string): Cell.Cell => ({
  schema: "cell.v1",
  id,
  case: `case-${id}`,
  lane,
  batch: "batch-1",
  attempt: 1,
  clock: "simulated",
  keys: { call_id: [`${id}@test`], caller: [], callee: [], request_uri_user: [] },
  legs: [],
  timeline: { start: { lane_ms: 0, wall_ms: 0 }, end: { lane_ms: 1000, wall_ms: 1000 } },
  claims: {},
  records: {},
  verifications: [],
  dir,
  tags: [],
  needs: []
})

/** A locker holding its locks in memory: a lock another holder has is refused, naming that holder. */
export const memoryLocker = (): CollectorRun.Locker => {
  const held = new Map<string, string>()
  return (names, holder) =>
    Effect.suspend(() => {
      const busy = names.find((name) => held.has(name))
      if (busy !== undefined) return Effect.fail(`${busy} is held by ${held.get(busy)}`)
      for (const name of names) held.set(name, holder)
      return Effect.succeed(Effect.sync(() => names.forEach((name) => held.delete(name))))
    })
}

/** Whether process `pid` still exists. */
export const alive = (pid: number): boolean => {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

/** Whether the trace under `root` holds `line` within `withinMs`. */
export const tracedWithin = async (root: string, line: string, withinMs = 5000): Promise<boolean> => {
  const deadline = Date.now() + withinMs
  while (!traceOf(root).includes(line)) {
    if (Date.now() > deadline) return false
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
  return true
}

/** Whether process `pid` is gone within `withinMs`, a killed one's reaping by its new parent waited for. */
export const goneWithin = async (pid: number, withinMs = 2000): Promise<boolean> => {
  const deadline = Date.now() + withinMs
  while (alive(pid)) {
    if (Date.now() > deadline) return false
    await new Promise((resolve) => setTimeout(resolve, 20))
  }
  return true
}

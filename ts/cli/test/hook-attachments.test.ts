/**
 * A job's hook logs land as its `log` attachments: listed in the job's
 * directory and added to the store under the job; a store that refuses them
 * leaves the job running, and no hook log attaches nothing.
 */
import { NodeServices } from "@effect/platform-node"
import { Attachment } from "@sip/contracts"
import { Attachments } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { ConnectionError, SqlError } from "effect/unstable/sql/SqlError"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { attachHooks } from "../src/hook-attachments.js"

let jobDir: string
beforeEach(() => {
  jobDir = fs.mkdtempSync(path.join(os.tmpdir(), "hook-attachments-"))
})
afterEach(() => {
  fs.rmSync(jobDir, { recursive: true, force: true })
})

/** A store that records what is added, or refuses every add where `refusing`. */
const storeOf = (added: Array<[Attachments.Holder, Attachments.Held]>, refusing = false) =>
  Layer.succeed(Attachments.Service)(Attachments.Service.of({
    add: (holder, held) =>
      refusing
        ? Effect.fail(new SqlError({ reason: new ConnectionError({ cause: new Error("refused"), message: "refused" }) }))
        : Effect.sync(() => void added.push([holder, held])),
    of: () => Effect.succeed([]),
    byId: () => Effect.succeed(undefined)
  }))

const LOGS = [
  { name: "before", exit: 0, path: "" },
  { name: "after", exit: 3, path: "" }
]

const logsIn = (dir: string) => LOGS.map((log) => ({ ...log, path: path.join(dir, "hooks", `${log.name}.log`) }))

const listedIn = (dir: string) =>
  fs.existsSync(path.join(dir, Attachment.FILE)) ? Attachment.parse(fs.readFileSync(path.join(dir, Attachment.FILE), "utf8")) : []

describe("a job's hook logs", () => {
  it("are listed in the job's directory and added to the store under the job", async () => {
    const added: Array<[Attachments.Holder, Attachments.Held]> = []
    await Effect.runPromise(
      attachHooks(42, jobDir, logsIn(jobDir)).pipe(Effect.provide([storeOf(added), NodeServices.layer]))
    )
    const expected = [
      { name: "hook-before", kind: "log", title: "hook before", file: "hooks/before.log", source: "hooks" },
      { name: "hook-after", kind: "log", title: "hook after", file: "hooks/after.log", source: "hooks" }
    ]
    expect(listedIn(jobDir)).toEqual(expected)
    expect(added).toEqual([[{ scope: "job", jobId: 42 }, { dir: jobDir, attachments: expected }]])
  })

  it("refused by the store leave the job's effect succeeding", async () => {
    const exit = await Effect.runPromise(
      Effect.exit(attachHooks(42, jobDir, logsIn(jobDir)).pipe(Effect.provide([storeOf([], true), NodeServices.layer])))
    )
    expect(exit._tag).toBe("Success")
    expect(listedIn(jobDir).map((one) => one.name)).toEqual(["hook-before", "hook-after"])
  })

  it("attach nothing where the job ran none", async () => {
    const added: Array<[Attachments.Holder, Attachments.Held]> = []
    await Effect.runPromise(attachHooks(42, jobDir, []).pipe(Effect.provide([storeOf(added), NodeServices.layer])))
    expect(added).toEqual([])
    expect(fs.existsSync(path.join(jobDir, Attachment.FILE))).toBe(false)
  })
})

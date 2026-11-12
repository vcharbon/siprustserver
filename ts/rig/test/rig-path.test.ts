/**
 * The rig path loads the pack, then the workspace, and refuses before any job
 * opens a manifest that does not hold, naming its file: a missing or
 * non-executable executable, a name defined in both roots (both files named),
 * a manifest whose name is not its directory's, a judge nobody defines, a
 * part schema that is no JSON object, a path `pack.toml` names that the
 * pack does not hold, a collector named like a source of the base, and a lock
 * collectors of nested scopes name.
 */
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as RigPath from "../src/rig-path.js"
import { collector, type Entry, pack, run, SCRIPT, tree } from "./tree.js"

const made: Array<string> = []

const root = (files: Readonly<Record<string, Entry>>): string => {
  const dir = tree(files)
  made.push(dir)
  return dir
}

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

const refusalOf = async (roots: RigPath.Roots) => {
  const result = await run(RigPath.load(roots))
  return Result.isFailure(result) ? result.failure : undefined
}

describe("the rig path", () => {
  it("holds the pack's and the workspace's plug-ins, each from its own file", async () => {
    const packDir = root({
      "pack.toml": pack(),
      "collectors/sut-log/collector.toml": collector("sut-log"),
      "collectors/sut-log/collect": SCRIPT,
      "judges/call-records/judge.toml": `schema = "judge.v1"\nname = "call-records"\nmodule = "./judge.ts"\n`,
      "judges/call-records/judge.ts": "export default {}\n",
      "parts/records-store.schema.json": `{"type": "object"}`
    })
    const workspace = root({
      "collectors/call-records/collector.toml": collector("call-records", `judge = "call-records"`),
      "collectors/call-records/collect": SCRIPT,
      "operations/restart-worker/operation.toml":
        `schema = "operation.v1"\nname = "restart-worker"\n[run]\ninject = "./inject"\nheal = "./heal"\n`,
      "operations/restart-worker/inject": SCRIPT,
      "operations/restart-worker/heal": SCRIPT
    })
    const result = await run(RigPath.load({ pack: packDir, workspace }))
    expect(Result.isSuccess(result)).toBe(true)
    if (!Result.isSuccess(result)) return
    const loaded = result.success
    expect([...loaded.collectors.values()].map((item) => [item.name, item.level, item.file])).toEqual([
      ["sut-log", "pack", path.join(packDir, "collectors/sut-log/collector.toml")],
      ["call-records", "workspace", path.join(workspace, "collectors/call-records/collector.toml")]
    ])
    expect([...loaded.judges.keys(), ...loaded.operations.keys(), ...loaded.parts.keys()]).toEqual([
      "call-records",
      "restart-worker",
      "records-store"
    ])
    expect(loaded.pack?.manifest.wrapper).toBe("rigref")
  })

  it("refuses a name the workspace defines again, naming both files", async () => {
    const packDir = root({
      "pack.toml": pack(),
      "collectors/sut-log/collector.toml": collector("sut-log"),
      "collectors/sut-log/collect": SCRIPT
    })
    const workspace = root({
      "collectors/sut-log/collector.toml": collector("sut-log"),
      "collectors/sut-log/collect": SCRIPT
    })
    const refusal = await refusalOf({ pack: packDir, workspace })
    expect(refusal?.files).toEqual([
      path.join(packDir, "collectors/sut-log/collector.toml"),
      path.join(workspace, "collectors/sut-log/collector.toml")
    ])
    expect(refusal?.reason).toBe("collector sut-log is defined twice")
  })

  it("refuses a part schema both roots define, naming both files", async () => {
    const packDir = root({ "pack.toml": pack(), "parts/records-store.schema.json": `{"type": "object"}` })
    const workspace = root({ "parts/records-store.schema.json": `{"type": "object"}` })
    expect((await refusalOf({ pack: packDir, workspace }))?.files).toEqual([
      path.join(packDir, "parts/records-store.schema.json"),
      path.join(workspace, "parts/records-store.schema.json")
    ])
  })

  it("refuses a missing executable, naming the manifest and the step", async () => {
    const workspace = root({ "collectors/sut-log/collector.toml": collector("sut-log") })
    const refusal = await refusalOf({ workspace })
    expect(refusal?.message).toBe(`${path.join(workspace, "collectors/sut-log/collector.toml")}: run.collect: ./collect does not exist`)
  })

  it("refuses an executable that is not executable", async () => {
    const workspace = root({
      "collectors/sut-log/collector.toml": collector("sut-log"),
      "collectors/sut-log/collect": "#!/bin/sh\n"
    })
    expect((await refusalOf({ workspace }))?.reason).toBe("run.collect: ./collect is not executable")
  })

  it("refuses a judge module that does not exist", async () => {
    const workspace = root({ "judges/call-records/judge.toml": `schema = "judge.v1"\nname = "call-records"\nmodule = "./judge.ts"\n` })
    expect((await refusalOf({ workspace }))?.reason).toBe("module: ./judge.ts does not exist")
  })

  it("refuses an operation whose heal is missing", async () => {
    const workspace = root({
      "operations/restart-worker/operation.toml":
        `schema = "operation.v1"\nname = "restart-worker"\n[run]\ninject = "./inject"\nheal = "./heal"\n`,
      "operations/restart-worker/inject": SCRIPT
    })
    expect((await refusalOf({ workspace }))?.reason).toBe("run.heal: ./heal does not exist")
  })

  it("refuses a manifest with an unknown field before checking anything it names", async () => {
    const workspace = root({ "collectors/sut-log/collector.toml": collector("sut-log", `poll = 1`) })
    expect((await refusalOf({ workspace }))?.message).toBe(
      `${path.join(workspace, "collectors/sut-log/collector.toml")}: poll: not a key of this file`
    )
  })

  it("refuses a manifest whose name is not its directory's", async () => {
    const workspace = root({ "collectors/sut-log/collector.toml": collector("sut-logs"), "collectors/sut-log/collect": SCRIPT })
    expect((await refusalOf({ workspace }))?.reason).toBe("name: sut-logs stands in the directory sut-log")
  })

  it("refuses a plug-in directory without its manifest, and a stray file among them", async () => {
    const empty = root({ "collectors/sut-log/collect": SCRIPT })
    expect((await refusalOf({ workspace: empty }))?.reason).toBe("holds no collector.toml")
    const stray = root({ "collectors/README.md": "notes\n" })
    expect((await refusalOf({ workspace: stray }))?.files).toEqual([path.join(stray, "collectors/README.md")])
  })

  it("refuses a collector whose judge no root defines", async () => {
    const workspace = root({
      "collectors/call-records/collector.toml": collector("call-records", `judge = "call-records"`),
      "collectors/call-records/collect": SCRIPT
    })
    expect((await refusalOf({ workspace }))?.reason).toBe("judge: names call-records, which no judge of the path defines")
  })

  it("refuses a part schema that is no JSON object", async () => {
    const workspace = root({ "parts/records-store.schema.json": "[]" })
    expect((await refusalOf({ workspace }))?.reason).toBe("a part schema is one JSON Schema object")
  })

  it("refuses a pack without pack.toml, and a path its manifest names that the pack does not hold", async () => {
    const bare = root({})
    expect((await refusalOf({ pack: bare }))?.reason).toBe("a pack holds pack.toml at its root")
    const named = root({ "pack.toml": pack(`[modules]\ndeployment = "./deployment.ts"\n`) })
    expect((await refusalOf({ pack: named }))?.message).toBe(
      `${path.join(named, "pack.toml")}: modules.deployment: the pack holds no file ./deployment.ts`
    )
  })

  it("refuses a collector named like a source the base answers lines as", async () => {
    const workspace = root({
      "collectors/interpreter/collector.toml": collector("interpreter", `answers = ["call"]`),
      "collectors/interpreter/collect": SCRIPT
    })
    const refusal = await refusalOf({ workspace })
    expect(refusal?.files).toEqual([path.join(workspace, "collectors/interpreter/collector.toml")])
    expect(refusal?.reason).toBe("name: interpreter is a source the base answers its own lines as")
  })

  it("refuses a lock named by a job and a batch collector, naming both manifests; one named by two batch collectors loads", async () => {
    const scoped = (name: string, scope: string) =>
      `schema = "collector.v1"\nname = "${name}"\nscope = "${scope}"\nlocks = ["lab"]\n[run]\ncollect = "./collect"\n`
    const nested = root({
      "collectors/job-watch/collector.toml": scoped("job-watch", "job"),
      "collectors/job-watch/collect": SCRIPT,
      "collectors/lab-log/collector.toml": scoped("lab-log", "batch"),
      "collectors/lab-log/collect": SCRIPT
    })
    const refusal = await refusalOf({ workspace: nested })
    expect(refusal?.files).toEqual([
      path.join(nested, "collectors/job-watch/collector.toml"),
      path.join(nested, "collectors/lab-log/collector.toml")
    ])
    expect(refusal?.reason).toMatch(/^locks: lab is named by the job collector job-watch and the batch collector lab-log, whose scopes nest/)
    const siblings = root({
      "collectors/lab-log/collector.toml": scoped("lab-log", "batch"),
      "collectors/lab-log/collect": SCRIPT,
      "collectors/lab-trace/collector.toml": scoped("lab-trace", "batch"),
      "collectors/lab-trace/collect": SCRIPT
    })
    expect(await refusalOf({ workspace: siblings })).toBeUndefined()
  })

  it("refuses a lock named by a batch and a cell collector", async () => {
    const workspace = root({
      "collectors/lab-log/collector.toml": collector("lab-log", `locks = ["lab"]`),
      "collectors/lab-log/collect": SCRIPT,
      "collectors/cell-probe/collector.toml":
        `schema = "collector.v1"\nname = "cell-probe"\nscope = "cell"\nlocks = ["lab"]\n[run]\ncollect = "./collect"\n`,
      "collectors/cell-probe/collect": SCRIPT
    })
    expect((await refusalOf({ workspace }))?.reason).toMatch(/^locks: lab is named by the cell collector cell-probe and the batch collector lab-log/)
  })

  it("loads an empty workspace and a pack with empty plug-in directories", async () => {
    const packDir = root({ "pack.toml": pack() })
    for (const dir of Object.values(RigPath.DIRS)) fs.mkdirSync(path.join(packDir, dir))
    const result = await run(RigPath.load({ pack: packDir, workspace: root({}) }))
    expect(Result.isSuccess(result) && result.success.collectors.size).toBe(0)
  })
})

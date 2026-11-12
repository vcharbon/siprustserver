/**
 * A collector written as one TypeScript module with `defineCollector`, run by
 * the collector runner as its manifest's executable for every step: the step
 * `SIPRIG_STEP` names runs, its state kept from `open` to `close`, its
 * attached files and lines written for it; a step that fails exits non-zero
 * with one line saying why, which its cells' `not checkable` quotes.
 */
import { Attachment } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import type * as Cell from "../src/cell.js"
import * as CollectorRun from "../src/collector-run.js"
import * as RigPath from "../src/rig-path.js"
import { cellOf, memoryLocker } from "./collectors.js"
import { laneOf } from "./operations.js"
import { run, tree } from "./tree.js"

const MODULE = path.join(import.meta.dirname, "fixtures", "defined-collector", "main.ts")
const PACKAGE = path.join(import.meta.dirname, "..")

const made: Array<string> = []

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A rig path whose collector `module` runs the fixture module for each of its steps; its root and the instance with `params`. */
const moduleCollector = async (params: Readonly<Record<string, unknown>>) => {
  const root = tree({
    "collectors/module/collector.toml": `schema = "collector.v1"\nname = "module"\nscope = "batch"\nanswers = ["module-check"]\n` +
      `[run]\nopen = "./run"\ncollect = "./run"\nclose = "./run"\n`,
    "collectors/module/run": {
      text: `#!/bin/sh\ncd "${PACKAGE}" && exec "${process.execPath}" --import tsx "${MODULE}"\n`,
      executable: true
    }
  })
  made.push(root)
  const loaded = await run(RigPath.load({ workspace: root }))
  if (!Result.isSuccess(loaded)) throw new Error(`the collector does not load: ${String(loaded.failure)}`)
  const instance: CollectorRun.Instance = {
    collector: loaded.success.collectors.get("module")!,
    id: "module",
    label: "module",
    scope: "batch",
    lane: laneOf("lane-a"),
    lanes: undefined,
    params: { trace: path.join(root, "trace"), ...params },
    dir: path.join(root, "batch", "collectors", "module"),
    holder: path.join(root, "batch")
  }
  const cells: ReadonlyArray<Cell.Cell> = ["c1", "c2"].map((id) => cellOf(id, "lane-a", path.join(root, "cells", id)))
  return { root, instance, cells }
}

const collect = (instance: CollectorRun.Instance, cells: ReadonlyArray<Cell.Cell>) =>
  Effect.runPromise(Effect.exit(Effect.scoped(Effect.flatMap(
    CollectorRun.opened([instance], { sink: () => Effect.void, locker: memoryLocker() }),
    (opened) => opened.collect(cells)
  ))))

describe("a collector defined as a module", () => {
  it("runs the step SIPRIG_STEP names, its attachments and answers written for it, its state kept from open to close", async () => {
    const { root, instance, cells } = await moduleCollector({})
    const exit = await collect(instance, cells)
    if (!Exit.isSuccess(exit)) throw new Error(String(exit.cause))
    const [collected] = exit.value
    for (const cell of cells) {
      const file = `collected/module/${cell.id}.log`
      expect(collected!.cells.get(cell.id)).toEqual({
        verifications: [{ name: "module-check", source: "module", answer: "pass" }],
        attachments: [{ name: "module", kind: "log", title: "its log", file, source: "module" }]
      })
      expect(Attachment.parse(fs.readFileSync(path.join(cell.dir, Attachment.FILE), "utf8")).map((listed) => listed.file)).toEqual([file])
      expect(fs.readFileSync(path.join(cell.dir, file), "utf8")).toBe(`open: ${cell.id}\n`)
    }
    expect(fs.readFileSync(path.join(root, "trace"), "utf8")).toBe("closed\n")
  })

  it("exits non-zero with one line saying why its step failed, which every cell's not checkable quotes", async () => {
    const { instance, cells } = await moduleCollector({ fail: true })
    const exit = await collect(instance, cells)
    if (!Exit.isSuccess(exit)) throw new Error(String(exit.cause))
    const log = path.join(instance.dir, "collect.log")
    for (const cell of cells) {
      expect(exit.value[0]!.cells.get(cell.id)!.verifications).toEqual([
        {
          name: "module-check",
          source: "module",
          answer: "not-checkable",
          message: `collect exited 1: the log source is gone, its log ${log}`
        }
      ])
    }
  })
})

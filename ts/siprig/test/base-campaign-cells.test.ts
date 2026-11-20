/**
 * The case sources a base campaign run names: a relative one is the pack's,
 * and is refused where it leads out of the pack's tree once its links are
 * followed, a link to a file outside included; a file whose name merely opens
 * with `..` stays the pack's.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { selectionOf } from "../src/base-campaign-cells.ts"

const work = fs.mkdtempSync(path.join(os.tmpdir(), "base-campaign-cells-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** A pack under `work` holding `..cases.ts`, a case source stating no cell, and `linked.ts`, a link to one outside it. */
const pack = (() => {
  const dir = path.join(work, "pack")
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, "..cases.ts"), "export default []\n")
  fs.writeFileSync(path.join(work, "outside.ts"), "export default []\n")
  fs.symlinkSync(path.join(work, "outside.ts"), path.join(dir, "linked.ts"))
  return dir
})()

/** The run's selection of `sources` in the pack, or its refusal's message. */
const selected = (sources: ReadonlyArray<string>) =>
  Effect.runPromise(Effect.result(selectionOf(pack, sources)).pipe(Effect.provide(NodeServices.layer))).then((result) =>
    Result.isSuccess(result) ? { files: result.success.files.map((file) => file.file) } : { refused: result.failure.message }
  )

describe("a base campaign run's named case sources", () => {
  it("takes a source whose name opens with .. as the pack's", async () => {
    expect(await selected(["..cases.ts"])).toEqual({ files: ["..cases.ts"] })
  })

  it("refuses a source linking out of the pack's tree, naming it", async () => {
    expect(await selected(["linked.ts"])).toEqual({ refused: `the case source linked.ts leads out of the pack ${pack}` })
  })
})

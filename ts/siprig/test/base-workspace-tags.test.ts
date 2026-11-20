/**
 * The campaign tags a base campaign run selects and schedules by: those the
 * workspace's `rig.toml` declares (`[tags]`, `[[tag_cases]]`), none where no
 * workspace is named or it holds no `rig.toml`; a `rig.toml` that does not
 * read refuses the run, naming the file.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { declaredOf } from "../src/base-workspace.ts"

const work = fs.mkdtempSync(path.join(os.tmpdir(), "base-workspace-tags-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** A workspace under `work` named `name`, its `rig.toml` holding `text` where one is given. */
const workspace = (name: string, text?: string): string => {
  const root = path.join(work, name)
  fs.mkdirSync(root, { recursive: true })
  if (text !== undefined) fs.writeFileSync(path.join(root, "rig.toml"), text)
  return root
}

const declared = (root: string | undefined) =>
  Effect.runPromise(Effect.result(declaredOf(root)).pipe(Effect.provide(NodeServices.layer)))

describe("a base campaign's declared tags", () => {
  it("are those the workspace's rig.toml declares, with the case files its tag_cases tag", async () => {
    const root = workspace("declaring", [
      "[tags.disruptive]",
      `label = "disrupts the system under test"`,
      `default = "run"`,
      "alone = true",
      "",
      "[[tag_cases]]",
      `tag = "disruptive"`,
      `cases = ["./restart-cases.ts"]`,
      ""
    ].join("\n"))
    expect(await declared(root)).toEqual(Result.succeed({
      file: path.join(root, "rig.toml"),
      tags: { disruptive: { label: "disrupts the system under test", default: "run", alone: true } },
      tagCases: [{ tag: "disruptive", cases: ["./restart-cases.ts"] }]
    }))
  })

  it("are none where no workspace is named, or its rig.toml is absent", async () => {
    const none = { file: "", tags: {}, tagCases: [] }
    expect(await declared(undefined)).toEqual(Result.succeed(none))
    expect(await declared(workspace("bare"))).toEqual(Result.succeed(none))
  })

  it("refuse the run where the workspace's rig.toml does not read, naming it", async () => {
    const root = workspace("broken", "[tags.disruptive]\nlabel = \"x\"\n")
    const refused = await declared(root)
    expect(Result.isFailure(refused) ? refused.failure : "read").toContain(path.join(root, "rig.toml"))
  })
})

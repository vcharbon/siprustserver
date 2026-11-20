/**
 * Outside vitest, a workspace file's SDK specifier resolves through the
 * hook to the tooling's own entry, from a directory holding no module of the
 * tooling; an unchanged workspace loads as the module already imported, and
 * a module of the directory the file imports is read anew once edited. A
 * file that does not load is refused naming it, its cause on one line.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { afterAll, describe, expect, it } from "vitest"
import { defaultExport, versionOf } from "../src/load.js"
import { carriedOnto, queryOf } from "../src/resolve-hook.js"

const HERE = path.dirname(fileURLToPath(import.meta.url))
const TSX = path.resolve(HERE, "../node_modules/.bin/tsx")

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "case-modules-load-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

describe("a workspace file loaded with its SDK", () => {
  it("resolves the SDK's specifier to the entry, and reads an edited module it imports anew", () => {
    const root = path.join(dir, "reread")
    fs.mkdirSync(path.join(root, "lib"), { recursive: true })
    fs.writeFileSync(path.join(root, "package.json"), `{ "type": "module" }\n`)
    fs.writeFileSync(path.join(root, "lib/name.ts"), `export const NAME = "alice"\n`)
    fs.writeFileSync(
      path.join(root, "case.ts"),
      `import { defineGreeting } from "@example/sdk"\nimport { NAME } from "./lib/name.js"\nexport default defineGreeting({ name: NAME, tone: "loud" })\n`
    )
    const out = execFileSync(TSX, [path.join(HERE, "bin/load-twice.ts"), root], {
      encoding: "utf8",
      env: { ...process.env, NODE_OPTIONS: "--disable-warning=DEP0205" }
    })
    expect(JSON.parse(out.trim().split("\n").at(-1)!)).toEqual({ first: "HELLO ALICE", same: true, edited: "HELLO BOB" })
  })

  it("is refused naming the file where it does not load", async () => {
    const root = path.join(dir, "throws")
    fs.mkdirSync(root, { recursive: true })
    const file = path.join(root, "case.ts")
    fs.writeFileSync(file, `throw new Error("no greeting\\nhere")\n`)
    const sdk = { specifier: "@example/sdk", entry: new URL("./fixtures/sdk/index.ts", import.meta.url).href }
    const refused = await Effect.runPromise(
      Effect.flatMap(versionOf(root), (stamp) => defaultExport(sdk, file, "throws/case.ts", stamp)).pipe(
        Effect.flip,
        Effect.provide(NodeServices.layer)
      )
    )
    expect(refused.message).toBe("throws/case.ts: does not load: no greeting here")
  })

  it("carries the directory's query onto its own modules only", () => {
    const parent = `file:///ws/campaigns/case.ts${queryOf("/ws/campaigns", "v1")}`
    expect(carriedOnto(parent, "file:///ws/campaigns/lib/numbers.ts")).toBe(`file:///ws/campaigns/lib/numbers.ts${queryOf("/ws/campaigns", "v1")}`)
    expect(carriedOnto(parent, "file:///ws/campaigns/node_modules/x/index.js")).toBeUndefined()
    expect(carriedOnto(parent, "file:///opt/tools/sdk.ts")).toBeUndefined()
    expect(carriedOnto(parent, "node:fs")).toBeUndefined()
    expect(carriedOnto("file:///ws/campaigns/case.ts", "file:///ws/campaigns/lib/numbers.ts")).toBeUndefined()
  })
})

/**
 * A workspace made ready holds every fixed subdirectory, the tooling's own
 * among them, but the postgres cluster's, the seeds private; the tooling's
 * placed file in a directory init makes alone; its configuration file from
 * the template; an empty census run where the census holds none; and keeps
 * whatever it already holds.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import * as Init from "../src/init.js"
import { PACK } from "./pack.js"

const NOW = Date.parse("2026-03-04T05:06:07Z")

const SEED: Init.Seed<"extras"> = {
  template: "# the tooling's file\n",
  placed: { extras: { name: "README.md", text: "Put extras here.\n" } }
}

let root: string
beforeEach(() => {
  root = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "workspace-init-")), "ws")
})
afterEach(() => fs.rmSync(path.dirname(root), { recursive: true, force: true }))

const prepare = () => Effect.runPromise(Init.prepare(PACK, SEED, root, NOW).pipe(Effect.provide(NodeServices.layer)))

describe("a workspace made ready", () => {
  it("makes every fixed subdirectory but pgdata, the tooling's own included, the seeds private", async () => {
    const made = await prepare()
    expect(fs.readdirSync(root).sort()).toEqual(
      [".seeds", "artifacts", "backups", "campaigns", "census", "extras", "inbox", "lanes", "locks", "logs", "pcaps", "runs", "state",
        "targets", "tool.toml"].sort()
    )
    expect(made.directories).toHaveLength(14)
    expect(fs.statSync(path.join(root, ".seeds")).mode & 0o777).toBe(0o700)
  })

  it("writes the template and an empty census run dated the day it runs, and places the tooling's file", async () => {
    const made = await prepare()
    expect(made.config).toBe(path.join(root, "tool.toml"))
    expect(fs.readFileSync(made.config!, "utf8")).toBe(SEED.template)
    expect(made.census).toBe(path.join(root, "census", "2026-03-04.census.json"))
    const census = JSON.parse(fs.readFileSync(made.census!, "utf8"))
    expect(census).toMatchObject({ documents: 0, captures: [], hits: [], failures: [] })
    expect(Object.values(census.rules).every((rule) => (rule as { readonly hits: number }).hits === 0)).toBe(true)
    expect(fs.readFileSync(path.join(root, "extras", "README.md"), "utf8")).toBe("Put extras here.\n")
  })

  it("keeps what the workspace already holds: its file, a census run, a directory and what is in it", async () => {
    fs.mkdirSync(path.join(root, "extras"), { recursive: true })
    fs.mkdirSync(path.join(root, "census"))
    fs.writeFileSync(path.join(root, "tool.toml"), "[site]\n")
    fs.writeFileSync(path.join(root, "census", "2026-01-01.census.json"), "{}\n")
    const made = await prepare()
    expect(made.config).toBeUndefined()
    expect(made.census).toBeUndefined()
    expect(made.directories).not.toContain(path.join(root, "extras"))
    expect(fs.readdirSync(path.join(root, "extras"))).toEqual([])
    expect(fs.readFileSync(path.join(root, "tool.toml"), "utf8")).toBe("[site]\n")
    const again = await prepare()
    expect(again.directories).toEqual([])
  })
})

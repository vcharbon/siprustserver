/**
 * The base's case generator (`src/cut/base-generator.ts`), run as a cut runs
 * it (`Generator.BASE`): a work list that does not read is refused whole, exit
 * 2 naming the list, and no capture gets a manifest line.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { BASE } from "../../src/cut/generator.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "base-generator-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The generator run over the work list at `list`: its exit status, its stderr, and whether it wrote a manifest. */
const generate = (list: string) => {
  const manifest = path.join(dir, `${path.basename(list)}.manifest.jsonl`)
  const [command, ...args] = BASE.argv
  const run = spawnSync(command!, [...args, "--work-list", list, "--manifest", manifest], { cwd: BASE.cwd, encoding: "utf8" })
  return { status: run.status, stderr: run.stderr, manifest: fs.existsSync(manifest) }
}

describe("the base generator", () => {
  it("refuses a work list that is no list of captures, exit 2, naming it", () => {
    const list = path.join(dir, "not-a-list.json")
    fs.writeFileSync(list, JSON.stringify({ capture: "one.pcap" }))
    const { status, stderr, manifest } = generate(list)
    expect([status, manifest]).toEqual([2, false])
    expect(stderr).toContain(`the work list ${list} does not read`)
  })
})

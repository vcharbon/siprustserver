/**
 * The base's case generator (`src/cut/base-generator.ts`), run as a cut runs
 * it (`Generator.BASE`): a work list that does not read is refused whole, exit
 * 2 naming the list, and no capture gets a manifest line; the Call-ID
 * derivation `--call-id-derivation` names joins a capture's legs alike in the
 * cut and in the assembly of its cases.
 */
import { Pivot } from "@sip/contracts"
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { serialHuntFlows } from "../../../pipeline/test/fixtures.js"
import { BASE } from "../../src/cut/generator.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "base-generator-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The generator run over the work list at `list`: its exit status, its stderr, and whether it wrote a manifest. */
const generate = (list: string, extra: ReadonlyArray<string> = []) => {
  const manifest = path.join(dir, `${path.basename(list)}.manifest.jsonl`)
  const [command, ...args] = BASE.argv
  const run = spawnSync(command!, [...args, "--work-list", list, "--manifest", manifest, ...extra], { cwd: BASE.cwd, encoding: "utf8" })
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

  it("assembles a serial hunt under counter-prefix as one called branch of two attempts", () => {
    const capture = "hunt.pcap"
    const srcDir = path.join(dir, "hunt", "src")
    const outDir = path.join(dir, "hunt", "out")
    fs.mkdirSync(srcDir, { recursive: true })
    fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify(serialHuntFlows()))
    const list = path.join(dir, "hunt.json")
    fs.writeFileSync(list, JSON.stringify([{ capture, srcDir, outDir }]))
    const { status, stderr } = generate(list, ["--call-id-derivation", "counter-prefix"])
    expect(status, stderr).toBe(0)
    const document = path.join(outDir, "cases", "hunt-1", "hunt-1.v3.json")
    const pivot = Pivot.decodePivotSync(JSON.parse(fs.readFileSync(document, "utf8")) as unknown)
    expect(pivot.calls.flatMap((call) => call.attempts).map((a) => [a.branch, a.position, a.final?.status])).toEqual([
      [0, 0, 486],
      [0, 1, 200]
    ])
    expect(pivot.calls[0]!.attempts[1]!.join_evidence).toBe(
      "the attempts' Call-IDs are application-server derivations of one base call"
    )
  })
})

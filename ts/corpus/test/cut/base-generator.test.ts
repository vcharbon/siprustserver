/**
 * The base's case generator (`src/cut/base-generator.ts`), run as a cut runs
 * it (`Generator.BASE`): a work list that does not read is refused whole, exit
 * 2 naming the list, and no capture gets a manifest line; the Call-ID
 * derivation `--call-id-derivation` names joins a capture's legs alike in the
 * cut and in the assembly of its cases; the leg rules `--correlation-rules`
 * names, the default set where none is, join legs beside it, and a document
 * that does not project their headers is not cut; a rule file that does not
 * read as leg rules is refused whole, exit 2 naming it.
 */
import { Pivot } from "@sip/contracts"
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { serialHuntFlows } from "../../../pipeline/test/fixtures.js"
import { calleeLeg, callerLeg, linkedDoc } from "../../../pipeline/test/linked-legs.js"
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

/** The default rule set's headers, which a flows document the base generator cuts under it projects. */
const DEFAULT_HEADERS = ["Call-ID", "X-CID"]

const CALLER = "caller-1@192.0.2.9"
const CALLEE = "callee-1@192.0.2.1"

/** The generator run over a capture `name` whose flows document is `flows`, with `extra` arguments: its manifest's lines and its out directory. */
const run = (name: string, flows: unknown, extra: ReadonlyArray<string>) => {
  const capture = `${name}.pcap`
  const srcDir = path.join(dir, name, "src")
  const outDir = path.join(dir, name, "out")
  fs.mkdirSync(srcDir, { recursive: true })
  fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify(flows))
  const list = path.join(dir, `${name}.json`)
  fs.writeFileSync(list, JSON.stringify([{ capture, srcDir, outDir }]))
  const { status, stderr } = generate(list, extra)
  expect(status, stderr).toBe(0)
  const manifest = path.join(dir, `${name}.json.manifest.jsonl`)
  const lines = fs.readFileSync(manifest, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as unknown)
  return { lines, outDir }
}

/** The cases the generator cut of a capture `name` whose flows document is `flows`, and each refusal as `<case> <reason>`. */
const casesOf = (name: string, flows: unknown, extra: ReadonlyArray<string>) => {
  const { outDir } = run(name, flows, extra)
  const excluded = path.join(outDir, "excluded.json")
  const refused = fs.existsSync(excluded)
    ? (JSON.parse(fs.readFileSync(excluded, "utf8")) as ReadonlyArray<{ caseId: string; reason: string }>).map((r) => `${r.caseId} ${r.reason}`)
    : []
  return { cases: fs.readdirSync(path.join(outDir, "cases")).sort(), refused: refused.sort() }
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
    fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify({ ...serialHuntFlows(), emit_headers: DEFAULT_HEADERS }))
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

  it("joins a callee leg to the caller leg its X-CID names under the default rule set, and keeps them apart under none", () => {
    const flows = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER, "X-CID")], DEFAULT_HEADERS)
    expect(casesOf("xcid", flows, []), "the default set: one case of both legs").toEqual({ cases: ["xcid-1"], refused: [] })
    const none = path.join(dir, "no.rules.json")
    fs.writeFileSync(none, JSON.stringify({ version: 0, rules: [] }))
    expect(
      casesOf("xcid-none", flows, ["--correlation-rules", none]),
      "no rule: the caller leg misses its dial, the callee leg its upstream leg"
    ).toEqual({
      cases: [],
      refused: ["xcid-none-1 source-dial-not-captured", "xcid-none-2 missing-upstream-leg"]
    })
  })

  it("does not cut a capture whose flows document does not project the default rules' headers, naming the re-emission", () => {
    const flows = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER, "X-CID")], [])
    const { lines } = run("unprojected", flows, [])
    expect(lines).toHaveLength(1)
    expect(lines[0]).toMatchObject({ capture: "unprojected.pcap", cut: false })
    expect(String((lines[0] as { error?: string }).error)).toContain("--emit-headers Call-ID,X-CID")
  })

  it("refuses a rule file holding a rule no cut reads, exit 2, naming it, and cuts no capture", () => {
    const rules = path.join(dir, "retry.rules.json")
    fs.writeFileSync(rules, JSON.stringify({ version: 0, rules: [{ name: "busy", kind: "retry", finals: ["486"], window_ms: 1000 }] }))
    const list = path.join(dir, "retry-list.json")
    fs.writeFileSync(list, JSON.stringify([]))
    const { status, stderr, manifest } = generate(list, ["--correlation-rules", rules])
    expect([status, manifest]).toEqual([2, false])
    expect(stderr).toContain(`the rule file ${rules} does not read`)
    expect(stderr).toContain("busy (retry)")
  })
})

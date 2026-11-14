/**
 * The base's case generator (`src/cut/base-generator.ts`), run as a cut runs
 * it (`Generator.BASE`): a work list that does not read is refused whole, exit
 * 2 naming the list, and no capture gets a manifest line; the Call-ID
 * derivation `--call-id-derivation` names joins a capture's legs alike in the
 * cut and in the assembly of its cases; the leg rules `--correlation-rules`
 * names join legs beside it, and a document that does not project their
 * headers is not cut; where none is named, the default set joins them, and a
 * document that does not project its headers is cut without it, its line
 * saying so; a rule file that does not read as leg rules is refused whole,
 * exit 2 naming it.
 */
import { Pivot } from "@sip/contracts"
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import { serialHuntFlows } from "../../../pipeline/test/fixtures.js"
import { calleeLeg, callerLeg, linkedDoc, linkedHuntFlows } from "../../../pipeline/test/linked-legs.js"
import { BASE } from "../../src/cut/generator.js"
import { DEFAULT_RULES } from "../../src/cut/leg-rules.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "base-generator-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** The generator run over the work list at `list`: its exit status, its stdout and stderr, and whether it wrote a manifest. */
const generate = (list: string, extra: ReadonlyArray<string> = []) => {
  const manifest = path.join(dir, `${path.basename(list)}.manifest.jsonl`)
  const [command, ...args] = BASE.argv
  const run = spawnSync(command!, [...args, "--work-list", list, "--manifest", manifest, ...extra], { cwd: BASE.cwd, encoding: "utf8" })
  return { status: run.status, stdout: run.stdout, stderr: run.stderr, manifest: fs.existsSync(manifest) }
}

/** The headers the default rule set reads off a projection: its `Call-ID` side is the leg's own. */
const DEFAULT_HEADERS = ["X-CID"]

const CALLER = "caller-1@192.0.2.9"
const CALLEE = "callee-1@192.0.2.1"

/** The generator run over a capture `name` whose flows document is `flows`, with `extra` arguments: its manifest's lines, its stdout and its out directory. */
const run = (name: string, flows: unknown, extra: ReadonlyArray<string>) => {
  const capture = `${name}.pcap`
  const srcDir = path.join(dir, name, "src")
  const outDir = path.join(dir, name, "out")
  fs.mkdirSync(srcDir, { recursive: true })
  fs.writeFileSync(path.join(srcDir, `${capture}.flows.json`), JSON.stringify(flows))
  const list = path.join(dir, `${name}.json`)
  fs.writeFileSync(list, JSON.stringify([{ capture, srcDir, outDir }]))
  const { status, stderr, stdout } = generate(list, extra)
  expect(status, stderr).toBe(0)
  const manifest = path.join(dir, `${name}.json.manifest.jsonl`)
  const lines = fs.readFileSync(manifest, "utf8").split("\n").filter((line) => line !== "").map((line) => JSON.parse(line) as unknown)
  return { lines, stdout, outDir }
}

/** The cases the generator cut of a capture `name` whose flows document is `flows`, each refusal as `<case> <reason>`, and its stdout. */
const casesOf = (name: string, flows: unknown, extra: ReadonlyArray<string>) => {
  const { outDir, stdout } = run(name, flows, extra)
  const excluded = path.join(outDir, "excluded.json")
  const refused = fs.existsSync(excluded)
    ? (JSON.parse(fs.readFileSync(excluded, "utf8")) as ReadonlyArray<{ caseId: string; reason: string }>).map((r) => `${r.caseId} ${r.reason}`)
    : []
  return { cases: fs.readdirSync(path.join(outDir, "cases")).sort(), refused: refused.sort(), stdout }
}

/** The one case `name` of a run's out directory, decoded. */
const pivotOf = (outDir: string, name: string) =>
  Pivot.decodePivotSync(JSON.parse(fs.readFileSync(path.join(outDir, "cases", name, `${name}.v3.json`), "utf8")) as unknown)

/** A rule file at `name` in the test directory holding `rules`. */
const ruleFile = (name: string, rules: ReadonlyArray<unknown>): string => {
  const file = path.join(dir, name)
  fs.writeFileSync(file, JSON.stringify({ version: 0, rules }))
  return file
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
    const { outDir } = run("hunt", { ...serialHuntFlows(), emit_headers: DEFAULT_HEADERS }, ["--call-id-derivation", "counter-prefix"])
    const pivot = pivotOf(outDir, "hunt-1")
    expect(pivot.calls.flatMap((call) => call.attempts).map((a) => [a.branch, a.position, a.final?.status])).toEqual([
      [0, 0, 486],
      [0, 1, 200]
    ])
    expect(pivot.calls[0]!.attempts[1]!.join_evidence).toBe(
      "the attempts' Call-IDs are application-server derivations of one base call"
    )
  })

  it("assembles a serial hunt whose attempts' X-CID names the caller leg as one branch, the evidence naming the rule", () => {
    const { outDir } = run("xcid-hunt", linkedHuntFlows("X-CID"), [])
    const pivot = pivotOf(outDir, "xcid-hunt-1")
    expect(pivot.calls.flatMap((call) => call.attempts).map((a) => [a.branch, a.position, a.final?.status])).toEqual([
      [0, 0, 486],
      [0, 1, 200]
    ])
    expect(pivot.calls[0]!.attempts[1]!.join_evidence).toBe(
      `the attempts were set up off one call, joined by the leg rule "asterisk-x-cid"`
    )
  })

  it("joins a callee leg to the caller leg its X-CID names under the default rule set, and keeps them apart under none", () => {
    const flows = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER, "X-CID")], DEFAULT_HEADERS)
    const joined = casesOf("xcid", flows, [])
    expect([joined.cases, joined.refused], "the default set: one case of both legs").toEqual([["xcid-1"], []])
    expect(joined.stdout, "the default rule applied: nothing skipped").not.toContain("is skipped")
    expect(
      casesOf("xcid-none", flows, ["--correlation-rules", ruleFile("no.rules.json", [])]),
      "no rule: the caller leg misses its dial, the callee leg its upstream leg"
    ).toMatchObject({
      cases: [],
      refused: ["xcid-none-1 source-dial-not-captured", "xcid-none-2 missing-upstream-leg"]
    })
  })

  it("cuts a capture not projecting the default rule's header without it, saying so once, naming the rule and both remedies", () => {
    const { lines, stdout, outDir } = run("unprojected", { ...serialHuntFlows(), emit_headers: [] }, ["--call-id-derivation", "counter-prefix"])
    expect(lines).toEqual([{ capture: "unprojected.pcap", cut: true }])
    expect(fs.readdirSync(path.join(outDir, "cases")), "the derivation alone joins its legs").toEqual(["unprojected-1"])
    const said = stdout.split("\n").filter((line) => line.includes("is skipped"))
    expect(said).toEqual([
      `[cut] unprojected.pcap: the default leg rule "asterisk-x-cid" is skipped: the flows document does not project X-CID; ` +
      "project it through the deployment's Tools.emitHeaders, or name a rule set with --correlation-rules"
    ])
  })

  it("does not cut a capture not projecting a header of a rule set named, naming Tools.emitHeaders", () => {
    const flows = linkedDoc([callerLeg(CALLER, 0), calleeLeg(CALLEE, 10, CALLER, "X-CID")], [])
    const { lines } = run("unprojected-named", flows, ["--correlation-rules", DEFAULT_RULES])
    expect(lines).toHaveLength(1)
    expect(lines[0]).toMatchObject({ capture: "unprojected-named.pcap", cut: false })
    expect(String((lines[0] as { error?: string }).error)).toContain(
      `the leg rule "asterisk-x-cid" reads X-CID: name X-CID in the deployment's Tools.emitHeaders`
    )
  })

  it("refuses a rule file holding a rule no cut reads, exit 2, naming it, and cuts no capture", () => {
    const rules = ruleFile("retry.rules.json", [{ name: "busy", kind: "retry", finals: ["486"], window_ms: 1000 }])
    const list = path.join(dir, "retry-list.json")
    fs.writeFileSync(list, JSON.stringify([]))
    const { status, stderr, manifest } = generate(list, ["--correlation-rules", rules])
    expect([status, manifest]).toEqual([2, false])
    expect(stderr).toContain(`the rule file ${rules} does not read`)
    expect(stderr).toContain("busy (retry)")
  })

  it("refuses a rule file whose pattern does not compile, exit 2, naming the rule and the side, and cuts no capture", () => {
    const rules = ruleFile("broken.rules.json", [{
      name: "broken",
      kind: "header-pair",
      left: { header: "Call-ID", pattern: "(?<key>[)" },
      right: { header: "X-CID", pattern: "^(?<key>\\S+)$" }
    }])
    const list = path.join(dir, "broken-list.json")
    fs.writeFileSync(list, JSON.stringify([{ capture: "broken.pcap", srcDir: path.join(dir, "broken", "src"), outDir: path.join(dir, "broken", "out") }]))
    const { status, stderr, manifest } = generate(list, ["--correlation-rules", rules])
    expect([status, manifest]).toEqual([2, false])
    expect(stderr).toContain(`the rule file ${rules} does not read`)
    expect(stderr).toContain(`rule "broken": "left.pattern" does not compile`)
  })
})

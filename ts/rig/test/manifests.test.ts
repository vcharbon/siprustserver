/**
 * Each manifest kind decodes the appendix's own example, and refuses, naming
 * its file and the dotted key, a field its schema does not name, another
 * schema version, and a value whose meaning does not hold.
 */
import * as Result from "effect/Result"
import { describe, expect, it } from "vitest"
import * as Collector from "../src/collector.js"
import * as Judge from "../src/judge.js"
import * as Operation from "../src/operation.js"
import * as Pack from "../src/pack.js"

const COLLECTOR = `schema   = "collector.v1"
name     = "call-records"
scope    = "batch"
at       = "after"
needs    = ["records-store"]
default  = "always"
required = false
after    = ["sut-log"]
answers  = ["call-records"]
judge    = "call-records"
locks    = ["records-chain"]

[params]
poll_s = 1

[join]
key = "call_id"

[timeouts]
open_s = 30
collect_s = 300
close_s = 30

[run]
open    = "./open"
collect = "./collect"
close   = "./close"
`

const refusalOf = <A>(result: Result.Result<A, { readonly message: string }>): string =>
  Result.isFailure(result) ? result.failure.message : "accepted"

describe("collector.v1", () => {
  it("decodes the appendix's manifest", () => {
    const parsed = Collector.parse("c/collector.toml", COLLECTOR)
    expect(Result.isSuccess(parsed) && Collector.executablesOf(parsed.success)).toEqual([
      ["open", "./open"],
      ["collect", "./collect"],
      ["close", "./close"]
    ])
  })

  it("refuses a field it does not name, naming the file and the key", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace("[join]", "[join]\nstrategy = \"hash\""))))
      .toBe("c/collector.toml: join.strategy: not a key of this file")
  })

  it("refuses another schema version", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace("collector.v1", "collector.v2"))))
      .toMatch(/^c\/collector\.toml: schema: /)
  })

  it("refuses a delay on a collector that is not of cell scope", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace("locks ", "delay_s = 10\nlocks "))))
      .toBe("c/collector.toml: delay_s: only a cell-scope collector is delayed")
  })

  it("refuses a box-wide lock on a collector of cell scope, accepting its delay", () => {
    const cell = COLLECTOR.replace(`scope    = "batch"`, `scope = "cell"\ndelay_s = 10`)
    expect(refusalOf(Collector.parse("c/collector.toml", cell)))
      .toBe("c/collector.toml: locks: a cell-scope collector takes no box-wide lock, which it would hold over the lane's next cells")
    expect(refusalOf(Collector.parse("c/collector.toml", cell.replace(`locks    = ["records-chain"]\n`, "")))).toBe("accepted")
  })

  it("refuses a pre-check that opens", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace(`at       = "after"`, `at = "before"`))))
      .toBe("c/collector.toml: run.open: a pre-check has no open")
  })

  it("refuses a pre-check naming a judge or a join", () => {
    const before = COLLECTOR.replace(`at       = "after"`, `at = "before"`).replace(`open    = "./open"\n`, "")
    expect(refusalOf(Collector.parse("c/collector.toml", before)))
      .toBe("c/collector.toml: judge: a pre-check's lines are its own, no judge decides them")
    expect(refusalOf(Collector.parse("c/collector.toml", before.replace(`judge    = "call-records"\n`, ""))))
      .toBe("c/collector.toml: join: a pre-check runs over no cell, it joins nothing")
    expect(refusalOf(Collector.parse("c/collector.toml", before.replace(`judge    = "call-records"\n`, "").replace(`[join]\nkey = "call_id"\n`, ""))))
      .toBe("accepted")
  })

  it("refuses a pre-check whose default is on-failure", () => {
    const before = COLLECTOR.replace(`at       = "after"`, `at = "before"`)
      .replace(`open    = "./open"\n`, "")
      .replace(`judge    = "call-records"\n`, "")
      .replace(`[join]\nkey = "call_id"\n`, "")
    expect(refusalOf(Collector.parse("c/collector.toml", before.replace(`default  = "always"`, `default = "on-failure"`))))
      .toBe("c/collector.toml: default: a pre-check runs before any cell failed, never on-failure")
    expect(refusalOf(Collector.parse("c/collector.toml", before.replace(`default  = "always"`, `default = "on-demand"`)))).toBe("accepted")
  })

  it("refuses a required collector whose default is not always", () => {
    const text = COLLECTOR.replace("required = false", "required = true").replace(`default  = "always"`, `default = "on-demand"`)
    expect(refusalOf(Collector.parse("c/collector.toml", text))).toBe("c/collector.toml: default: a required collector runs always, not on-demand")
  })

  it("refuses an absolute executable path", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace(`collect = "./collect"`, `collect = "/usr/bin/true"`))))
      .toMatch(/^c\/collector\.toml: run\.collect: /)
  })

  it("refuses an executable path resolving above the manifest's directory", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", COLLECTOR.replace(`collect = "./collect"`, `collect = "./bin/../../collect"`))))
      .toBe("c/collector.toml: run.collect: a path inside the manifest's directory, relative to it")
    expect(Result.isSuccess(Collector.parse("c/collector.toml", COLLECTOR.replace(`collect = "./collect"`, `collect = "./bin/../collect"`))))
      .toBe(true)
  })

  it("refuses text that is no TOML", () => {
    expect(refusalOf(Collector.parse("c/collector.toml", "schema = "))).toMatch(/^c\/collector\.toml: not TOML: /)
  })
})

describe("judge.v1", () => {
  const judge = (body: string) => `schema = "judge.v1"\nname = "call-records"\n${body}\n[timeouts]\njudge_s = 120\n`

  it("decodes a module judge and an executable one", () => {
    expect(Result.isSuccess(Judge.parse("j/judge.toml", judge(`module = "./judge.ts"`)))).toBe(true)
    expect(Result.isSuccess(Judge.parse("j/judge.toml", judge(`run = "./judge"`)))).toBe(true)
  })

  it("refuses a judge that is both a module and an executable, or neither", () => {
    expect(refusalOf(Judge.parse("j/judge.toml", judge(`module = "./judge.ts"\nrun = "./judge"`))))
      .toBe("j/judge.toml: module, run: a judge is a module or an executable, not both")
    expect(refusalOf(Judge.parse("j/judge.toml", judge("")))).toBe("j/judge.toml: module, run: a judge names its module or its executable")
  })

  it("refuses a field it does not name", () => {
    expect(refusalOf(Judge.parse("j/judge.toml", judge(`run = "./judge"\nlanguage = "sh"`))))
      .toBe("j/judge.toml: language: not a key of this file")
  })
})

describe("operation.v1", () => {
  const OPERATION = `schema = "operation.v1"
name   = "restart-worker"
needs  = ["orchestrator"]
locks  = []
[params]
grace_s = 5
[timeouts]
inject_s = 60
heal_s = 120
probe_s = 10
[run]
inject = "./inject"
heal   = "./heal"
probe  = "./probe"
`

  it("decodes the appendix's manifest", () => {
    const parsed = Operation.parse("o/operation.toml", OPERATION)
    expect(Result.isSuccess(parsed) && Operation.executablesOf(parsed.success).map(([step]) => step)).toEqual(["inject", "heal", "probe"])
  })

  it("refuses an operation with no heal", () => {
    expect(refusalOf(Operation.parse("o/operation.toml", OPERATION.replace(`heal   = "./heal"\n`, ""))))
      .toMatch(/^o\/operation\.toml: run\.heal: /)
  })

  it("refuses a field it does not name", () => {
    expect(refusalOf(Operation.parse("o/operation.toml", OPERATION.replace("[run]", "[run]\nretry = \"./retry\""))))
      .toBe("o/operation.toml: run.retry: not a key of this file")
  })
})

describe("pack.v1", () => {
  const PACK = `schema  = "pack.v1"
name    = "reference"
title   = "siprig reference"
wrapper = "rigref"
[image]
packages = []
binaries = "./bin"
[provision]
run = ""
[cases]
sources = ["./cases.ts"]
[modules]
deployment = "./deployment.ts"
lanes = "./lanes"
`

  it("decodes the appendix's manifest, its component defaulting to its name", () => {
    const parsed = Pack.parse("pack.toml", PACK)
    expect(Result.isSuccess(parsed) && Pack.componentOf(parsed.success)).toBe("reference")
  })

  it("states the paths that decide an extraction and a cut, and the component name", () => {
    const placed = Pack.parse("pack.toml", PACK.replace(`wrapper = "rigref"`, `wrapper = "rigref"\ncomponent = "ref"`) + `[decides]\nextraction = ["./anonymiser"]\ncut = ["./rules"]\n`)
    expect(Result.isSuccess(placed) && [Pack.componentOf(placed.success), Pack.pathsOf(placed.success).map((named) => named.key)]).toEqual([
      "ref",
      ["cases.sources", "modules.deployment", "modules.lanes", "decides.extraction", "decides.cut"]
    ])
  })

  it("refuses a field it does not name", () => {
    expect(refusalOf(Pack.parse("pack.toml", PACK.replace("[modules]", "[modules]\nsite = \"./site.ts\""))))
      .toBe("pack.toml: modules.site: not a key of this file")
  })

  it("refuses a decides path resolving above the pack's directory", () => {
    expect(refusalOf(Pack.parse("pack.toml", `${PACK}[decides]\ncut = ["./rules", "../../etc"]\n`)))
      .toBe("pack.toml: decides.cut: a path inside the manifest's directory, relative to it")
  })
})

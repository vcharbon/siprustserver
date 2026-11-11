/**
 * The lanes of a root load against the rig path: each lane file in `lane.v1`,
 * each part it names validated against its kind's schema, each operation it
 * binds resolved, its capabilities derived from its clock, its backend mode,
 * its parts and its bindings. A file at fault refuses, naming the file and the
 * dotted field; the lanes of one job naming two files of a shared kind refuse.
 */
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { afterEach, describe, expect, it } from "vitest"
import * as Lanes from "../src/lanes.js"
import type { Refusal } from "../src/manifest-file.js"
import * as RigPath from "../src/rig-path.js"
import { type Entry, run, SCRIPT, tree } from "./tree.js"

const made: Array<string> = []

const root = (files: Readonly<Record<string, Entry>>): string => {
  const dir = tree(files)
  made.push(dir)
  return dir
}

afterEach(() => {
  for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true })
})

/** A records store: where a system writes its records, shared by the lanes of a job, readable once it names a reader. */
const RECORDS_STORE = JSON.stringify({
  "x-siprig": { shared: true, capabilities: { "records-readable": { reader: true }, "records-writable": { mode: ["append", "reset"] } } },
  type: "object",
  additionalProperties: false,
  required: ["address"],
  properties: {
    address: { type: "string", pattern: "^[^\\s:]+:\\d{1,5}$" },
    mode: { enum: ["read-only", "append", "reset"] },
    reader: {
      type: "object",
      additionalProperties: false,
      required: ["user"],
      properties: { user: { type: "string", minLength: 1 }, password: { type: "string" } }
    }
  }
})

/** A worker pool: the processes a system runs on, one per lane. */
const WORKER_POOL = JSON.stringify({ type: "object", required: ["size"], properties: { size: { type: "integer", minimum: 1 } } })

const OPERATION = `schema = "operation.v1"\nname = "restart-worker"\nneeds = ["worker-pool"]\n[run]\ninject = "./inject"\nheal = "./heal"\n`

/** A workspace holding the part schemas, one operation, and `files`. */
const workspace = (files: Readonly<Record<string, Entry>>) =>
  root({
    "parts/records-store.schema.json": RECORDS_STORE,
    "parts/worker-pool.schema.json": WORKER_POOL,
    "operations/restart-worker/operation.toml": OPERATION,
    "operations/restart-worker/inject": SCRIPT,
    "operations/restart-worker/heal": SCRIPT,
    ...files
  })

const REAL = (mode: string, extra = ""): string =>
  `schema = "lane.v1"\nclock = "real"\n${extra}\n[sip]\nlifecycle = "standing"\ningress = "192.0.2.10:5060"\nendpoint_host = "192.0.2.1"\n` +
  `[backend]\nmode = "${mode}"\n${mode === "none" ? "" : `listen = "192.0.2.1:8080"\n`}` +
  `[backend.http_budget]\nnew_call_ms = 200\nfollow_up_ms = 200\ntransit_ms = 1\n`

const SIMULATED = (extra = ""): string => `schema = "lane.v1"\nclock = "simulated"\n${extra}\n`

const STORE_A = `address = "192.0.2.20:6379"\nmode = "append"\n[reader]\nuser = "reader"\n`

const loaded = (dir: string) =>
  run(Effect.flatMap(RigPath.load({ workspace: dir }), (rig) => Effect.map(Lanes.load(rig, dir), (lanes) => ({ rig, lanes }))))

const refusalOf = async (dir: string): Promise<Refusal | undefined> => {
  const result = await loaded(dir)
  return Result.isFailure(result) ? result.failure : undefined
}

const lanesOf = async (dir: string) => {
  const result = await loaded(dir)
  if (Result.isFailure(result)) throw new Error(result.failure.message)
  return result.success
}

describe("the lanes of a root", () => {
  it("derive each lane's capabilities from its clock, its backend mode, its parts and its bindings", async () => {
    const dir = workspace({
      "lanes/served.toml": REAL("served", `order = 2\n[parts]\nrecords-store = "a"\nworker-pool = "small"\n[operations]\nrestart = "restart-worker"`),
      "lanes/proxied.toml": REAL("proxied", `order = 1\n[parts]\nrecords-store = "b"`),
      "lanes/off-path.toml": REAL("none"),
      "lanes/in-process.toml": SIMULATED(`[parts]\nrecords-store = "a"`),
      "targets/records-store/a.toml": STORE_A,
      "targets/records-store/b.toml": `address = "192.0.2.21:6379"\n`,
      "targets/worker-pool/small.toml": `size = 2\n`
    })
    const { lanes } = await lanesOf(dir)
    expect(lanes.map((lane) => [lane.id, [...lane.capabilities].sort()])).toEqual([
      ["proxied", ["real-clock", "routing-visible"]],
      ["served", ["operation:restart", "real-clock", "records-readable", "records-writable", "routing-visible", "scripted-routing"]],
      ["in-process", ["paused-clock", "records-readable", "records-writable", "routing-visible", "scripted-routing"]],
      ["off-path", ["real-clock"]]
    ])
    const served = lanes.find((lane) => lane.id === "served")
    expect(served?.parts.get("records-store")).toEqual({
      kind: "records-store",
      name: "a",
      file: path.join(dir, "targets/records-store/a.toml"),
      value: { address: "192.0.2.20:6379", mode: "append", reader: { user: "reader" } }
    })
    expect(served?.operations.get("restart")?.file).toBe(path.join(dir, "operations/restart-worker/operation.toml"))
  })

  it("refuses a part file that breaks its schema, naming the file and the field", async () => {
    const lane = { "lanes/served.toml": REAL("served", `[parts]\nrecords-store = "a"`) }
    const file = (dir: string) => path.join(dir, "targets/records-store/a.toml")
    const badPort = workspace({ ...lane, "targets/records-store/a.toml": `address = "192.0.2.20"\n` })
    const refusal = await refusalOf(badPort)
    expect(refusal?.files).toEqual([file(badPort)])
    expect(refusal?.reason).toMatch(/^address: Expected a string matching/)
    const nested = workspace({ ...lane, "targets/records-store/a.toml": `address = "h:1"\n[reader]\npassword = "x"\n` })
    expect((await refusalOf(nested))?.message).toBe(`${file(nested)}: reader.user: Missing key`)
    const excess = workspace({ ...lane, "targets/records-store/a.toml": `address = "h:1"\ncolour = "blue"\n` })
    expect((await refusalOf(excess))?.message).toBe(`${file(excess)}: colour: not a key of this file`)
    const notToml = workspace({ ...lane, "targets/records-store/a.toml": `address = \n` })
    expect((await refusalOf(notToml))?.reason).toMatch(/^not TOML: /)
  })

  it("refuses a part kind the path has no schema for, and a part file that is not there", async () => {
    const unknownKind = workspace({ "lanes/served.toml": REAL("served", `[parts]\nqueue = "a"`) })
    expect((await refusalOf(unknownKind))?.message).toBe(
      `${path.join(unknownKind, "lanes/served.toml")}: parts.queue: no part schema queue.schema.json on the path`
    )
    const missing = workspace({ "lanes/served.toml": REAL("served", `[parts]\nrecords-store = "c"`) })
    expect((await refusalOf(missing))?.reason).toBe(`parts.records-store: no part file ${path.join(missing, "targets/records-store/c.toml")}`)
  })

  it("refuses a binding to an operation the path does not define, or whose parts the lane lacks", async () => {
    const undefinedOne = workspace({ "lanes/served.toml": REAL("served", `[operations]\nrestart = "reboot"`) })
    expect((await refusalOf(undefinedOne))?.reason).toBe("operations.restart: names reboot, which no operation of the path defines")
    const lacking = workspace({ "lanes/served.toml": REAL("served", `[operations]\nrestart = "restart-worker"`) })
    expect((await refusalOf(lacking))?.files).toEqual([
      path.join(lacking, "lanes/served.toml"),
      path.join(lacking, "operations/restart-worker/operation.toml")
    ])
  })

  it("refuses a lane file that breaks lane.v1, naming the dotted key", async () => {
    const reasonOf = async (text: string) => (await refusalOf(workspace({ "lanes/one.toml": text })))?.reason
    expect(await reasonOf(REAL("served", `colour = "blue"`))).toBe("colour: not a key of this file")
    expect(await reasonOf(REAL("served").replace(`ingress = "192.0.2.10:5060"`, `ingress = "192.0.2.10"`))).toMatch(/^sip\.ingress: /)
    expect(await reasonOf(`schema = "lane.v1"\nclock = "real"\n`)).toBe("sip: a real lane states its SIP target")
    expect(await reasonOf(REAL("served").replace(`clock = "real"`, `clock = "simulated"`))).toBe(
      "sip: a simulated lane's system runs in process, on no SIP target"
    )
    expect(await reasonOf(REAL("served").replace(`listen = "192.0.2.1:8080"\n`, ""))).toBe(
      "backend.listen: a lane whose backend is served states where the rig listens for it"
    )
    expect(await reasonOf(REAL("none").replace(`mode = "none"\n`, `mode = "none"\nlisten = "192.0.2.1:8080"\n`))).toBe(
      "backend.listen: a lane whose backend is none has no rig on its system's backend path"
    )
    expect(await reasonOf(SIMULATED(`[tags]\nrun = ["slow"]\nexclude = ["slow"]`))).toBe("tags: slow stands in both run and exclude")
    expect(await reasonOf(SIMULATED(`[collectors]\nexclude = "sut-log"`))).toMatch(/^collectors\.exclude: /)
    expect(await reasonOf(SIMULATED().replace("lane.v1", "lane.v2"))).toMatch(/^schema: /)
  })

  it("refuses a lane id that is not a name", async () => {
    const dir = workspace({ "lanes/Lab_A.toml": SIMULATED() })
    expect((await refusalOf(dir))?.reason).toBe("a lane id is lower-case letters, digits and dashes")
  })
})

describe("the parts the lanes of one job share", () => {
  const job = async (files: Readonly<Record<string, Entry>>) => {
    const { rig, lanes } = await lanesOf(workspace({
      "targets/records-store/a.toml": STORE_A,
      "targets/records-store/b.toml": `address = "192.0.2.21:6379"\n`,
      "targets/worker-pool/small.toml": `size = 2\n`,
      "targets/worker-pool/large.toml": `size = 8\n`,
      ...files
    }))
    return { lanes, shared: Lanes.sharedOf(rig, lanes) }
  }

  it("refuses two lanes naming two files of a shared kind, naming the lanes", async () => {
    const { lanes, shared } = await job({
      "lanes/one.toml": REAL("served", `[parts]\nrecords-store = "a"`),
      "lanes/two.toml": REAL("proxied", `[parts]\nrecords-store = "b"`)
    })
    expect(Result.isFailure(shared) && shared.failure.files).toEqual(lanes.map((lane) => lane.file))
    expect(Result.isFailure(shared) && shared.failure.reason).toBe("the lanes of one job name one records-store part: one names a, two names b")
  })

  it("refuses a lane naming none where another names one", async () => {
    const { shared } = await job({ "lanes/one.toml": REAL("served", `[parts]\nrecords-store = "a"`), "lanes/two.toml": SIMULATED() })
    expect(Result.isFailure(shared) && shared.failure.reason).toBe(
      "the lanes of one job name one records-store part: one names a, two names no records-store part"
    )
  })

  it("refuses a lane naming no part where another names a part file called none", async () => {
    const { shared } = await job({
      "targets/records-store/none.toml": STORE_A,
      "lanes/one.toml": REAL("served", `[parts]\nrecords-store = "none"`),
      "lanes/two.toml": SIMULATED()
    })
    expect(Result.isFailure(shared) && shared.failure.reason).toBe(
      "the lanes of one job name one records-store part: one names none, two names no records-store part"
    )
  })

  it("holds one file of a shared kind, and different files of a kind not shared", async () => {
    const { shared } = await job({
      "lanes/one.toml": REAL("served", `[parts]\nrecords-store = "a"\nworker-pool = "small"`),
      "lanes/two.toml": SIMULATED(`[parts]\nrecords-store = "a"\nworker-pool = "large"`)
    })
    expect(Result.isSuccess(shared) && [...shared.success].map(([kind, part]) => [kind, part?.name])).toEqual([["records-store", "a"]])
  })
})

describe("a part schema the base reads", () => {
  it("refuses an x-siprig the base does not read, and a root that is no object schema", async () => {
    const annotated = root({ "parts/records-store.schema.json": JSON.stringify({ type: "object", "x-siprig": { shared: "yes" } }) })
    const refusal = await run(RigPath.load({ workspace: annotated }))
    expect(Result.isFailure(refusal) && refusal.failure.message).toBe(
      `${path.join(annotated, "parts/records-store.schema.json")}: x-siprig.shared: Expected boolean`
    )
    const scalar = root({ "parts/records-store.schema.json": JSON.stringify({ type: "string" }) })
    const second = await run(RigPath.load({ workspace: scalar }))
    expect(Result.isFailure(second) && second.failure.reason).toBe(`type: a part schema's root is "object"`)
  })

  const reasonOf = async (schema: Readonly<Record<string, unknown>>) => {
    const loaded = await run(RigPath.load({ workspace: root({ "parts/records-store.schema.json": JSON.stringify(schema) }) }))
    return Result.isFailure(loaded) ? loaded.failure.reason : undefined
  }

  it("refuses a capability the base derives itself, so no part grants a lane one", async () => {
    expect(await reasonOf({ type: "object", "x-siprig": { capabilities: { "scripted-routing": {} } } })).toBe(
      "x-siprig.capabilities.scripted-routing: the base derives scripted-routing itself, no part gives it"
    )
    expect(await reasonOf({ type: "object", "x-siprig": { capabilities: { "operation:restart-worker": {} } } })).toBe(
      "x-siprig.capabilities.operation:restart-worker: a capability is lower-case letters, digits and dashes"
    )
  })

  it.each([
    [{ type: "object", if: { required: ["a"] }, then: { required: ["b"] } }, "/if"],
    [{ type: "object", dependentRequired: { a: ["b"] } }, "/dependentRequired"],
    [{ type: "object", properties: { a: { type: "string", format: "email" } } }, "/properties/a/format"],
    [{ type: "object", properties: { "a/b": { not: { type: "null" } } } }, "/properties/a~1b/not"],
    [{ type: "object", properties: { a: { anyOf: [{ type: "string" }, { contains: { type: "string" } }] } } }, "/properties/a/anyOf/1/contains"]
  ])("refuses a keyword the rig does not read, naming its JSON Pointer (%j)", async (schema, pointer) => {
    expect(await reasonOf(schema)).toBe(`${pointer}: a keyword the rig does not read`)
  })

  it("reads every keyword it checks a part by", async () => {
    expect(await reasonOf({
      $schema: "https://json-schema.org/draft/2020-12/schema",
      title: "every keyword",
      type: "object",
      additionalProperties: false,
      required: ["a"],
      minProperties: 1,
      maxProperties: 9,
      propertyNames: { pattern: "^[a-z]+$" },
      $defs: { port: { type: "integer", minimum: 1, maximum: 65535, multipleOf: 1 } },
      properties: {
        a: { $ref: "#/$defs/port" },
        b: { type: "string", minLength: 1, maxLength: 9, description: "b" },
        c: { type: "array", items: { enum: ["x", "y"] }, minItems: 1, maxItems: 2, uniqueItems: true },
        d: { allOf: [{ type: "number", exclusiveMinimum: 0 }, { exclusiveMaximum: 9 }] },
        e: { oneOf: [{ const: 1 }, { type: "boolean", default: false }] },
        f: { type: "array", prefixItems: [{ type: "string" }] },
        g: { type: "object", patternProperties: { "^x-": { type: "string" } } }
      }
    })).toBeUndefined()
  })
})

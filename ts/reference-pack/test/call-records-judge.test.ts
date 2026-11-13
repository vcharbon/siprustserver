/**
 * The `call-records` judge over the records joined to each cell: `pass`
 * where the cell holds the count of records it claims and each holds the
 * fields it claims, else `fail`. To each claiming cell it attaches its
 * records, flattened to one field per value (`records`), and the claimed
 * count and fields against the joined ones (`diff`, a row per one that
 * differs, `same` counting the others).
 */
import type { JudgeRun } from "@sip/rig"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import judge from "../judges/call-records/judge.ts"

const work = fs.mkdtempSync(path.join(os.tmpdir(), "call-records-judge-"))
afterAll(() => fs.rmSync(work, { recursive: true, force: true }))

/** A record as the B2BUA writes one, the fields the judge reads. */
const RECORD = {
  call_ref: "w0|a84b4c76e66710@pc33.atlanta.example.com|1928301774",
  a_leg: { call_id: "a84b4c76e66710@pc33.atlanta.example.com", state: "terminated" },
  b_legs: [{ leg_id: "b-1", disposition: "bridged" }],
  termination: { cause: "remote_bye", by_leg: "a" }
}

const FIELDS = { "termination.cause": "remote_bye", "b_legs.0.disposition": "bridged" }

/** A cell `id` claiming `claim`, `records` joined to it in its directory. */
const cellOf = (id: string, claim: unknown, records: ReadonlyArray<unknown>) => {
  const dir = path.join(work, id)
  fs.mkdirSync(path.join(dir, "joined"), { recursive: true })
  fs.writeFileSync(path.join(dir, "joined", "call-records.jsonl"), records.map((record) => `${JSON.stringify(record)}\n`).join(""))
  return { id, dir, claims: { "call-records": claim }, records: { "call-records": "joined/call-records.jsonl" } }
}

/** The judge's lines over `cells`, its files written under a directory of their own. */
const judged = async (cells: ReadonlyArray<ReturnType<typeof cellOf>>) => {
  const out = fs.mkdtempSync(path.join(work, "out-"))
  const context = { cells } as unknown as JudgeRun.Input["context"]
  const lines = [...(await judge({ context, out }))] as ReadonlyArray<Record<string, any>>
  return { out, lines }
}

/** The line answering `cell`'s verification. */
const verdictOf = (lines: ReadonlyArray<Record<string, any>>, cell: string) =>
  lines.find((line) => line["verification"] === "call-records" && line["cell"] === cell)

/** The file `cell`'s attachment of `kind` names, read under `out`. */
const attachedOf = (out: string, lines: ReadonlyArray<Record<string, any>>, cell: string, kind: string) => {
  const line = lines.find((one) => one["cell"] === cell && one["attachment"]?.["kind"] === kind)
  expect(line, `${cell}: a ${kind} attachment`).toBeDefined()
  return { name: line!["attachment"]["name"], text: fs.readFileSync(path.join(out, line!["attachment"]["file"]), "utf8") }
}

describe("the call-records judge", () => {
  it("passes a cell holding its claimed record and fields, attaching the record flattened and an empty diff", async () => {
    const { out, lines } = await judged([cellOf("alice", { count: 1, fields: FIELDS }, [RECORD])])
    expect(verdictOf(lines, "alice")).toMatchObject({ answer: "pass", message: "1 record(s), 1 claimed" })
    const records = attachedOf(out, lines, "alice", "records")
    expect(records.name).toBe("call-records")
    expect(records.text.split("\n").filter((line) => line !== "").map((line) => JSON.parse(line))).toEqual([{
      call_ref: RECORD.call_ref,
      "a_leg.call_id": RECORD.a_leg.call_id,
      "a_leg.state": "terminated",
      "b_legs.0.leg_id": "b-1",
      "b_legs.0.disposition": "bridged",
      "termination.cause": "remote_bye",
      "termination.by_leg": "a"
    }])
    const diff = attachedOf(out, lines, "alice", "diff")
    expect(diff.name).toBe("call-records-diff")
    expect(JSON.parse(diff.text)).toEqual({ columns: ["claimed", "joined"], rows: [], same: 3 })
  })

  it("fails a cell whose record differs in a claimed field, the diff naming that field alone", async () => {
    const claim = { count: 1, fields: { ...FIELDS, "termination.cause": "remote_final" } }
    const { out, lines } = await judged([cellOf("busy", claim, [RECORD])])
    expect(verdictOf(lines, "busy")).toMatchObject({ answer: "fail" })
    expect(verdictOf(lines, "busy")!["message"]).toContain("termination.cause")
    expect(JSON.parse(attachedOf(out, lines, "busy", "diff").text)).toEqual({
      columns: ["claimed", "joined"],
      rows: [{ field: "termination.cause", values: ["remote_final", "remote_bye"] }],
      same: 2
    })
  })

  it("fails a cell holding no record, its count and each claimed field against nothing", async () => {
    const { out, lines } = await judged([cellOf("lost", { count: 1, fields: FIELDS }, [])])
    expect(verdictOf(lines, "lost")).toMatchObject({ answer: "fail", detail: { expected: "1", seen: "0" } })
    expect(JSON.parse(attachedOf(out, lines, "lost", "diff").text)).toEqual({
      columns: ["claimed", "joined"],
      rows: [
        { field: "count", values: ["1", "0"] },
        { field: "termination.cause", values: ["remote_bye", null] },
        { field: "b_legs.0.disposition", values: ["bridged", null] }
      ],
      same: 0
    })
  })

  it("answers a claim stating no whole count not checkable, and gives a cell claiming nothing no line", async () => {
    const { lines } = await judged([cellOf("vague", { fields: FIELDS }, [RECORD]), { ...cellOf("quiet", undefined, []), claims: {} }])
    expect(verdictOf(lines, "vague")).toMatchObject({ answer: "not-checkable" })
    expect(lines.filter((line) => line["cell"] === "quiet")).toEqual([])
  })
})

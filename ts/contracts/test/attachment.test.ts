/**
 * A holder's attachment list as its writers append it: each name in the
 * order it first came, as its last line states it; a line that is no
 * attachment, or a file outside the holder's directory, is none.
 */
import { describe, expect, it } from "vitest"
import * as Attachment from "../src/attachment.js"

const LOG: Attachment.Attachment = { name: "sut-log", kind: "log", file: "sut.log", source: "replay" }
const RECORDS: Attachment.Attachment = { name: "records", kind: "records", title: "records", file: "r.jsonl", source: "judge" }

describe("Attachment.parse", () => {
  it("states each name once, where it first came, as its last line has it", () => {
    const text = [LOG, RECORDS, { ...LOG, file: "sut-2.log" }].map(Attachment.line).join("")
    expect(Attachment.parse(text)).toEqual([{ ...LOG, file: "sut-2.log" }, RECORDS])
  })

  it("skips a line that does not decode, a kind it does not know, and a file outside the holder's directory", () => {
    const text = [
      "{not json\n",
      `${JSON.stringify({ ...LOG, kind: "video" })}\n`,
      Attachment.line({ ...LOG, name: "up", file: "../x.log" }),
      Attachment.line({ ...LOG, name: "deep-up", file: "a/../../x.log" }),
      Attachment.line({ ...LOG, name: "absolute", file: "/etc/passwd" }),
      Attachment.line({ ...LOG, name: "empty", file: "" }),
      Attachment.line({ name: "console", kind: "link", file: "https://lab.example/c?x=../y", source: "pack" }),
      Attachment.line({ ...LOG, name: "nested", file: "logs/sut.log" })
    ].join("")
    expect(Attachment.parse(text).map((one) => one.name)).toEqual(["console", "nested"])
  })

  it("reads nothing from an empty list", () => {
    expect(Attachment.parse("")).toEqual([])
  })
})

describe("the kinds' file formats", () => {
  it("reads a table back as written, each value as text, a line that is no array or no JSON skipped", () => {
    const text = Attachment.tableText({ columns: ["call", "seen"], rows: [["A", "2"]] })
    expect(Attachment.tableOf(`${text}{"x":1}\nnot json\n[1,null,{"a":2}]\n`)).toEqual({
      columns: ["call", "seen"],
      rows: [["A", "2"], ["1", "", "{\"a\":2}"]]
    })
    expect(Attachment.tableOf("")).toEqual({ columns: [], rows: [] })
  })

  it("reads a table's cell classes back as written, a class it does not know as a plain cell", () => {
    const table: Attachment.Table = {
      columns: ["call", "expected", "seen"],
      rows: [["A", "2", "2"], ["B", "1", "0"]],
      classes: [[null, null, "ok"], [null, null, "nok"]]
    }
    const text = Attachment.tableText(table)
    expect(text.split("\n")[2]).toBe(`["B","1",{"value":"0","class":"nok"}]`)
    expect(Attachment.tableOf(text)).toEqual(table)
    expect(Attachment.tableOf(`["c"]\n[{"value":"1","class":"loud"}]\n`)).toEqual({ columns: ["c"], rows: [["1"]] })
  })

  it("reads records back as written, each object line one, its values as text", () => {
    const text = Attachment.recordsText([{ ID: "a@h", N: "1" }])
    expect(Attachment.recordsOf(`${text}[1]\n{"N":2,"M":null}\n`)).toEqual([{ ID: "a@h", N: "1" }, { N: "2", M: "" }])
  })

  it("reads a diff with its rows' classes and its quiet classes, and nothing else as one", () => {
    const diff = {
      columns: ["a", "b"],
      rows: [{ group: "#1", field: "T", values: ["1", null], class: "timing" }],
      same: 3,
      quiet: ["timing"]
    }
    expect(Attachment.diffOf(diff)).toEqual(diff)
    expect(Attachment.diffOf({ columns: ["a"], rows: [{ field: 1 }] })).toBeUndefined()
  })
})

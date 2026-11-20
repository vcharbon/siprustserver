/**
 * The lines of a collector's `collect`: each kind read by the key only it
 * holds, a line that is none refused with its number and why, blank lines
 * skipped.
 */
import * as Result from "effect/Result"
import { describe, expect, it } from "vitest"
import * as Lines from "../src/collector-lines.js"

const read = (text: string) => Lines.parse(text, "its stdout")

describe("a collector's lines", () => {
  it("are read by kind, each with where it was, blank lines skipped", () => {
    const text = [
      `{"record": {"id": "r1"}, "key": "a@b"}`,
      "",
      `{"record": {"id": "r2"}, "cell": "c1"}`,
      `{"verification": "count", "cell": "c1", "answer": "fail", "message": "2 records", "detail": {"expected": "1", "seen": "2"}}`,
      `{"key": "trunk", "cell": "c1", "values": ["t1", "t2"]}`,
      `{"attachment": {"kind": "records", "title": "Records", "file": "c1.jsonl"}, "cell": "c1"}`
    ].join("\n")
    expect(read(text)).toEqual(Result.succeed([
      { at: "its stdout line 1", line: { _tag: "KeyedRecord", line: { record: { id: "r1" }, key: "a@b" } } },
      { at: "its stdout line 3", line: { _tag: "AssignedRecord", line: { record: { id: "r2" }, cell: "c1" } } },
      {
        at: "its stdout line 4",
        line: {
          _tag: "Verification",
          line: { verification: "count", cell: "c1", answer: "fail", message: "2 records", detail: { expected: "1", seen: "2" } }
        }
      },
      { at: "its stdout line 5", line: { _tag: "Key", line: { key: "trunk", cell: "c1", values: ["t1", "t2"] } } },
      {
        at: "its stdout line 6",
        line: { _tag: "Attachment", line: { attachment: { kind: "records", title: "Records", file: "c1.jsonl" }, cell: "c1" } }
      }
    ]))
  })

  it.each([
    [`[1, 2]`, "its stdout line 1: not a JSON object"],
    [`{"hello": 1}`, "its stdout line 1: no record, verification, values or attachment key: not a line of a collector"],
    [`{"verification": "count", "answer": "pending"}`, /^its stdout line 1: answer: /],
    [`{"record": {}, "key": "k", "cell": "c1"}`, "its stdout line 1: key: not a key of this file"],
    [`{"attachment": {"kind": "picture", "file": "a.png"}}`, /^its stdout line 1: attachment\.kind: /]
  ])("refuses %s", (text, why) => {
    const parsed = read(text)
    expect(Result.isFailure(parsed)).toBe(true)
    if (Result.isFailure(parsed)) expect(parsed.failure).toMatch(why)
  })

  it("writes a line as it reads back", () => {
    const line: Lines.Line = { _tag: "Key", line: { key: "trunk", cell: "c1", values: ["t1"] } }
    expect(read(Lines.textOf(line))).toEqual(Result.succeed([{ at: "its stdout line 1", line }]))
  })
})

/**
 * One line a batch process writes, read three ways: a protocol line, a
 * protocol line that does not decode (malformed, naming its key and the
 * run-spec it names where it names one), or the process's own talk.
 */
import { describe, expect, it } from "vitest"
import { readLine } from "../src/batch.js"

describe("Batch.readLine", () => {
  it("reads each protocol line", () => {
    expect(readLine(`{"ready":{"bl":"127.0.0.1:9"}}`)).toEqual({ _tag: "line", line: { ready: { bl: "127.0.0.1:9" } } })
    expect(readLine(`{"cell":{"spec":"/s/a.json","exit":0,"summary":"a: Ok"}}`)).toEqual({
      _tag: "line",
      line: { cell: { spec: "/s/a.json", exit: 0, summary: "a: Ok" } }
    })
    expect(readLine(`{"end":{"dirty":null,"contexts":null}}`)).toEqual({
      _tag: "line",
      line: { end: { dirty: null, contexts: null } }
    })
  })

  it("reads a protocol key whose value does not decode as malformed, with the run-spec it names", () => {
    const cell = readLine(`{"cell":{"spec":"/s/b.json","exit":"zero"}}`)
    expect(cell).toMatchObject({ _tag: "malformed", key: "cell", spec: "/s/b.json" })
    expect(readLine(`{"cell":{"exit":0}}`)).toMatchObject({ _tag: "malformed", key: "cell", spec: undefined })
    expect(readLine(`{"end":{"dirty":3}}`)).toMatchObject({ _tag: "malformed", key: "end", spec: undefined })
  })

  it("reads anything else as the process's own talk", () => {
    expect(readLine("batch: signalled; ending")).toEqual({ _tag: "talk" })
    expect(readLine(`{"other":1}`)).toEqual({ _tag: "talk" })
    expect(readLine(`[1,2]`)).toEqual({ _tag: "talk" })
    expect(readLine("42")).toEqual({ _tag: "talk" })
  })
})

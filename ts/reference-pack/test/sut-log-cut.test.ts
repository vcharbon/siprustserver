/**
 * The `sut-log` collector's cut: a cell owns the entries stamped within its
 * window and those naming one of its Call-IDs; an unstamped line rides with
 * the entry before it.
 */
import { describe, expect, it } from "vitest"
import { cut, entriesOf } from "../collectors/sut-log/cut.ts"

const LOG = [
  "2026-01-01T00:00:01.000Z  INFO listening UDP",
  "2026-01-01T00:00:02.500Z  INFO a call came call_id=a84b4c76e66710@pc33.atlanta.example.com",
  "  its continuation",
  "2026-01-01T00:00:05.000Z  WARN a late word on call_id=a84b4c76e66710@pc33.atlanta.example.com",
  "2026-01-01T00:00:06.000Z  INFO draining",
  ""
].join("\n")

const at = (iso: string) => Date.parse(iso)

describe("the sut-log cut", () => {
  it("gives a cell the entries of its window, continuations kept, and those naming its Call-ID", () => {
    const window = { startMs: at("2026-01-01T00:00:02Z"), endMs: at("2026-01-01T00:00:03Z"), callIds: ["a84b4c76e66710@pc33.atlanta.example.com"] }
    expect(cut(entriesOf(LOG), window)).toBe(
      "2026-01-01T00:00:02.500Z  INFO a call came call_id=a84b4c76e66710@pc33.atlanta.example.com\n  its continuation\n" +
        "2026-01-01T00:00:05.000Z  WARN a late word on call_id=a84b4c76e66710@pc33.atlanta.example.com\n"
    )
  })

  it("gives a cell nothing where no entry falls in its window or names its calls", () => {
    const window = { startMs: at("2026-01-01T00:00:03Z"), endMs: at("2026-01-01T00:00:04Z"), callIds: ["f81d4fae-7dec@biloxi.example.com"] }
    expect(cut(entriesOf(LOG), window)).toBe("")
  })
})

/**
 * The schedule-table mirror. The oracle for the BYTES is the binary, held in
 * `@sip/toolchain`'s conformance test; what this package can say on its own is
 * that the checked-in table decodes under the strict contract and holds the
 * shape a finite, RFC-walked ladder must.
 */
import { describe, expect, it } from "vitest"
import { format } from "../src/canonical.js"
import { ClassName, decodeScheduleTableSync, rungGapsMs, rungIntervalsMs, SCHEDULES } from "../src/schedules.js"

describe("the schedule table", () => {
  const table = decodeScheduleTableSync(SCHEDULES)

  it("names every class once, in the order sip_retransmit::Class::ALL states them", () => {
    expect(table.classes.map((r) => r.class)).toEqual([...ClassName.literals])
  })

  it("walks each class to its give-up and no further", () => {
    for (const row of table.classes) {
      const elapsed = row.rung_intervals_ms.reduce((a, b) => a + b, 0)
      expect(elapsed, row.class).toBeLessThan(row.give_up_ms)
      expect(row.rung_intervals_ms.length, row.class).toBeGreaterThan(0)
    }
  })

  it("answers a class's rungs by name", () => {
    expect(rungIntervalsMs("final-2xx").slice(0, 4)).toEqual([500, 1000, 2000, 4000])
    expect(rungIntervalsMs("invite-client")).toEqual([500, 1000, 2000, 4000, 8000, 16000])
  })

  it("paces a message's rungs on its stated gaps, else on the class schedule", () => {
    expect(rungGapsMs("final-2xx", 2, [469, 998])).toEqual([469, 998])
    expect(rungGapsMs("final-2xx", 2, [])).toEqual([500, 1000])
    expect(rungGapsMs("final-2xx", 3)).toEqual([500, 1000, 2000])
    expect(rungGapsMs("final-2xx", 0, [469])).toEqual([])
    // A count past either list repeats the last gap.
    expect(rungGapsMs("final-2xx", 3, [469])).toEqual([469, 469, 469])
    expect(rungGapsMs("invite-client", 8)).toEqual([500, 1000, 2000, 4000, 8000, 16000, 16000, 16000])
    // The schedule itself lists no rung at or past the give-up.
    expect(rungIntervalsMs("final-2xx")[10]).toBeUndefined()
  })

  it("round-trips through the canonical bytes the binary writes", () => {
    const text = format(SCHEDULES)
    expect(format(decodeScheduleTableSync(JSON.parse(text) as unknown))).toBe(text)
  })

  it("refuses an unknown class or a spare field", () => {
    const row = SCHEDULES.classes[0]!
    expect(() => decodeScheduleTableSync({ classes: [{ ...row, class: "timer-j" }] })).toThrow()
    expect(() => decodeScheduleTableSync({ classes: [{ ...row, spare: 1 }] })).toThrow()
  })
})

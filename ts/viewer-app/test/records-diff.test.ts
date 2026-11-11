/**
 * The records of one name compared between cells: records align by
 * position; a field alike in every column holding the record is counted, not
 * listed; one that differs is a row with every column's value; a record a
 * column lacks is one `record` row; a column holding no attachment stands out
 * of every comparison.
 */
import { describe, expect, it } from "vitest"
import { recordsDiff } from "../src/records-diff.ts"

describe("records compared between cells", () => {
  it("lists each field that differs by record, counts the fields alike", () => {
    expect(recordsDiff(["bye", "cancel"], [
      [{ ID: "a", SIDE: "left", TYPE: "1" }],
      [{ ID: "a", SIDE: "right", TYPE: "1", EXTRA: "x" }]
    ])).toEqual({
      columns: ["bye", "cancel"],
      rows: [
        { group: "#1", field: "SIDE", values: ["left", "right"] },
        { group: "#1", field: "EXTRA", values: ["", "x"] }
      ],
      same: 2
    })
  })

  it("states a record a column lacks as one row, and leaves out a column with no attachment", () => {
    expect(recordsDiff(["a", "b", "none"], [
      [{ N: "1" }, { N: "2" }],
      [{ N: "1" }],
      null
    ])).toEqual({
      columns: ["a", "b", "none"],
      rows: [{ group: "#2", field: "record", values: ["present", "absent", null] }],
      same: 1
    })
  })

  it("reads nothing where no column holds a record", () => {
    expect(recordsDiff(["a", "b"], [null, []])).toEqual({ columns: ["a", "b"], rows: [], same: 0 })
  })
})

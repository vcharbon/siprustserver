/**
 * Which cells a capture's study lists and which one it opens on. A run set
 * ASIDE is opened by a link naming its cell and never by default: it is no
 * latest run, no entry of the run picker until it is the one on screen, and
 * no call of the capture's call bar.
 */
import { describe, expect, it } from "vitest"
import type { TriageStudyCell, TriageStudySession } from "../src/api.ts"
import { studyCells } from "../src/study-cells.ts"

const cell = (name: string, passed: boolean): TriageStudyCell =>
  ({ name, lane: "fake", passed }) as unknown as TriageStudyCell

const session = (name: string, cells: ReadonlyArray<TriageStudyCell>, aside?: string): TriageStudySession => ({
  name,
  dir: `/w/cap/${name}`,
  campaign: null,
  cells,
  hooks: [],
  ...(aside === undefined ? {} : { aside })
})

const SESSIONS = [
  session("run1", [cell("cap-k1__pivot-replay__fake", false)]),
  session("run2", [cell("cap-k1__pivot-replay__fake", true)], "a forced replay")
]

describe("a study's cells", () => {
  it("opens on the latest run that is not set aside", () => {
    expect(studyCells(SESSIONS, "fake", undefined, undefined).active?.key).toBe("run1/cap-k1__pivot-replay__fake")
  })

  it("opens on the latest such run of the call a study is of", () => {
    expect(studyCells(SESSIONS, "fake", undefined, "cap-k1").active?.key).toBe("run1/cap-k1__pivot-replay__fake")
  })

  it("opens an aside run's cell its link names, and lists it only then", () => {
    const named = studyCells(SESSIONS, "fake", "run2/cap-k1__pivot-replay__fake", "cap-k1")
    expect(named.active?.key).toBe("run2/cap-k1__pivot-replay__fake")
    expect(named.active?.session.aside).toBe("a forced replay")
    expect(named.picker.map((entry) => entry.key)).toEqual([
      "run1/cap-k1__pivot-replay__fake",
      "run2/cap-k1__pivot-replay__fake"
    ])
    expect(studyCells(SESSIONS, "fake", undefined, undefined).picker.map((entry) => entry.key)).toEqual([
      "run1/cap-k1__pivot-replay__fake"
    ])
  })

  it("draws the call bar from the latest run that is not set aside", () => {
    expect(studyCells(SESSIONS, "fake", "run2/cap-k1__pivot-replay__fake", undefined).latest.map((entry) => entry.key))
      .toEqual(["run1/cap-k1__pivot-replay__fake"])
  })

  it("opens nothing where every run is set aside and no link names one", () => {
    expect(studyCells([SESSIONS[1]!], "fake", undefined, "cap-k1").active).toBeUndefined()
  })
})

/**
 * Where a campaign's cells land, and what the driver asks the interpreter for.
 */
import { Bundle, Campaign } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { cellDir, cellIdOf, runSpecFile } from "../src/layout.js"
import { emitRunSpec } from "../src/run-spec.js"

const REPLAY: Campaign.PivotReplayCell = {
  kind: "pivot-replay",
  case: "/corpus/cases/transparent-defect.v3.json",
  lane: "some-lane"
}

const RUST: Campaign.RustTestCell = {
  kind: "rust-test",
  crate: "demo-e2e",
  name: "bc_02::relay_transparent"
}

describe("the cell coordinate", () => {
  it("names the case, the shape and the infra that ran it", () => {
    expect(cellIdOf(REPLAY)).toEqual({
      case: "transparent-defect.v3",
      shape: "pivot-replay",
      infra: "some-lane"
    })
    expect(cellIdOf(RUST)).toEqual({
      case: "bc_02::relay_transparent",
      shape: "rust-test",
      infra: "demo-e2e"
    })
  })

  it("takes a case directory's own name where the cell names one", () => {
    expect(Campaign.cellCaseId({ ...REPLAY, case: "/corpus/cases/some-case/" })).toBe("some-case")
  })

  it("puts the two shapes in distinct directories of one run", () => {
    expect(cellDir(REPLAY)).toBe("transparent-defect.v3__pivot-replay__some-lane")
    expect(cellDir(RUST)).toBe("bc_02::relay_transparent__rust-test__demo-e2e")
    expect(cellDir(REPLAY)).not.toBe(cellDir(RUST))
  })

  it("keeps the run-spec's name clear of the cell directory it points at", () => {
    // The interpreter WIPES `out_dir`, so a spec written inside it would be gone
    // before anyone could read what the run was asked to do.
    expect(runSpecFile(REPLAY)).toBe(`${cellDir(REPLAY)}.run-spec.json`)
  })
})

describe("the run-spec", () => {
  it("passes the lane block through verbatim and encodes the overlay", () => {
    const text = emitRunSpec({
      case: REPLAY.case,
      out_dir: "/runs/1/cell",
      lane: { kind: "fake", egress_endpoint: "e1", no_answer_ms: 12_000 },
      run: { ...Bundle.emptyOverlay, injected_headers: { "X-Thing": "1" } }
    })
    expect(JSON.parse(text)).toEqual({
      case: REPLAY.case,
      out_dir: "/runs/1/cell",
      lane: { kind: "fake", egress_endpoint: "e1", no_answer_ms: 12_000 },
      run: { timing_tolerance_ms: 0, injected_headers: { "X-Thing": "1" } }
    })
  })

  it("is canonical, so two identical runs diff to nothing", () => {
    const spec = {
      case: "c.json",
      out_dir: "/runs/1/cell",
      lane: { kind: "fake" },
      run: Bundle.emptyOverlay
    }
    const text = emitRunSpec(spec)
    expect(text.endsWith("\n")).toBe(true)
    expect(text.indexOf(`"case"`)).toBeLessThan(text.indexOf(`"lane"`))
    expect(text.indexOf(`"lane"`)).toBeLessThan(text.indexOf(`"out_dir"`))
    expect(emitRunSpec(spec)).toBe(text)
  })
})

/**
 * The run-bundle mirrors against the Rust crate's committed bundle fixtures.
 *
 * Same oracle as the document: the bytes. `verdict-failed.json` carries one of
 * every {@link Failure} variant, so decoding it is what proves the tagged union
 * is complete rather than merely plausible.
 */
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import {
  absorbsTiming,
  bindingHolds,
  clockCompresses,
  decodeRecordedMessageSync,
  decodeRunConfigSync,
  decodeRunRfcAuditSync,
  decodeRunTimingSync,
  decodeRunVerdictSync,
  emitRecordedMessage,
  emitRunConfig,
  emitRunRfcAudit,
  emitRunTiming,
  emitRunVerdict,
  Failure,
  failureStep,
  headersForCall,
  mediaModeOf,
  resolveBinding,
  rfcAuditPassed,
  rfcGating,
  verdictPassed
} from "../src/bundle/index.js"
import { PIVOT_FIXTURES, read } from "./fixtures.js"

const BUNDLE = path.join(PIVOT_FIXTURES, "bundle")

describe("byte round trips", () => {
  it("run-config.json", () => {
    const text = read(BUNDLE, "run-config.json")
    expect(emitRunConfig(decodeRunConfigSync(JSON.parse(text) as unknown))).toBe(text)
  })

  it("timing.json", () => {
    const text = read(BUNDLE, "timing.json")
    expect(emitRunTiming(decodeRunTimingSync(JSON.parse(text) as unknown))).toBe(text)
  })

  it.each(["verdict-ok.json", "verdict-failed.json", "verdict-negative.json"])("%s", (name) => {
    const text = read(BUNDLE, name)
    expect(emitRunVerdict(decodeRunVerdictSync(JSON.parse(text) as unknown))).toBe(text)
  })

  it.each(["rfc.json", "rfc-not-audited.json"])("%s", (name) => {
    const text = read(BUNDLE, name)
    expect(emitRunRfcAudit(decodeRunRfcAuditSync(JSON.parse(text) as unknown))).toBe(text)
  })
})

describe("the post-run RFC audit", () => {
  it("gates on the gating findings alone, and an advisory one is kept", () => {
    const audit = decodeRunRfcAuditSync(JSON.parse(read(BUNDLE, "rfc.json")) as unknown)
    expect(audit.status).toBe("audited")
    expect(rfcGating(audit).map((f) => f.rule)).toEqual(["cseq-in-dialog-order"])
    expect(rfcAuditPassed(audit)).toBe(false)
  })

  it("reads not-audited as its own value, never as a clean audit", () => {
    const audit = decodeRunRfcAuditSync(JSON.parse(read(BUNDLE, "rfc-not-audited.json")) as unknown)
    expect(audit.status).toBe("not-audited")
    expect(rfcGating(audit)).toEqual([])
    expect(rfcAuditPassed(audit)).toBe(true)
  })

  it("recording-A.jsonl, one line at a time", () => {
    const lines = read(BUNDLE, "recording-A.jsonl").split("\n").filter((line) => line.trim().length > 0)
    expect(lines.length).toBe(4)
    for (const line of lines) {
      expect(emitRecordedMessage(decodeRecordedMessageSync(JSON.parse(line) as unknown))).toBe(line)
    }
  })
})

describe("the failure vocabulary", () => {
  const failed = decodeRunVerdictSync(JSON.parse(read(BUNDLE, "verdict-failed.json")) as unknown)

  it("holds one of every member the union declares", () => {
    const seen = new Set((failed.failures ?? []).map((f) => f.failure))
    const declared = new Set(Failure.members.map((member) => member.fields.failure.literal as string))
    expect([...declared].filter((tag) => !seen.has(tag as never))).toEqual([])
  })

  it("names the run's failed step at the first failure that names one", () => {
    expect(failed.failed_step).toBe("s7")
    expect(failureStep((failed.failures ?? [])[0])).toBeUndefined()
    expect(verdictPassed(failed)).toBe(false)
  })

  it("carries a deviation-unimplemented step as a stated value, not an absent key", () => {
    const deviation = (failed.failures ?? []).find((f) => f.failure === "deviation-unimplemented")
    expect(deviation).toBeDefined()
    expect(failureStep(deviation!)).toBe("s5")
    // The serde `Option` has no `skip_serializing_if`, so `null` is a legal value here.
    const withNull = decodeRunVerdictSync({
      case: "c",
      lane: "l",
      status: "failed",
      failures: [{ failure: "deviation-unimplemented", deviation: "d1", kind: "raw-order", step: null, reason: "x" }]
    })
    expect(failureStep((withNull.failures ?? [])[0])).toBeUndefined()
    expect(emitRunVerdict(withNull)).toContain('"step": null')
  })
})

describe("a negative run", () => {
  const negative = decodeRunVerdictSync(JSON.parse(read(BUNDLE, "verdict-negative.json")) as unknown)

  it("passes by failing exactly as declared", () => {
    expect(negative.status).toBe("ok-negative")
    expect(verdictPassed(negative)).toBe(true)
    expect(negative.must_fail?.length).toBe(2)
    expect(negative.tolerated?.length).toBe(1)
  })

  it("lists a scripted peer's violation without gating on it", () => {
    expect(negative.rfc_violations?.[0].gating).toBe(false)
  })
})

describe("the run configuration", () => {
  const config = decodeRunConfigSync(JSON.parse(read(BUNDLE, "run-config.json")) as unknown)

  it("keeps a directive per call and lets no neighbour inherit it", () => {
    expect(headersForCall(config, "c1")["X-Api-Call"]).toBe('{"destination":"called-0-0"}')
    expect(headersForCall(config, "c2")["X-Api-Call"]).toBe('{"destination":"called-1-0"}')
    expect(headersForCall(config, "c3")).toEqual({})
  })

  it("accepts either side of a declared dwell and nothing wider", () => {
    expect(config.timing_tolerance_ms).toBe(700)
    expect(absorbsTiming(config, 15_000, 15_700)).toBe(true)
    expect(absorbsTiming(config, 15_000, 14_300)).toBe(true)
    expect(absorbsTiming(config, 15_000, 15_701)).toBe(false)
    expect(clockCompresses(config.clock)).toBe(true)
  })

  it("refuses a form the lane did not bind, by name", () => {
    const bindings = config.identities ?? {}
    expect(resolveBinding(bindings, "caller", "private")).toBe("0009001")
    expect(resolveBinding(bindings, "caller", "cnam")).toEqual({
      _tag: "unbound-form",
      name: "caller",
      form: "cnam",
      bound: ["e164", "private"]
    })
    expect(resolveBinding(bindings, "transferee", "e164")).toEqual({ _tag: "unbound-identity", name: "transferee" })
    expect(bindingHolds(bindings, "called-0-0")).toBe(true)
  })

  it("refuses an unknown knob rather than dropping it into a default", () => {
    expect(() =>
      decodeRunConfigSync({ lane: "upstream-fake", clock: "virtual", route_target: "h:1", lame: true })
    ).toThrow()
  })

  it("reads the media plane the run took, and an unstated one as rebooked", () => {
    const stated = decodeRunConfigSync({ lane: "upstream-fake", clock: "virtual", route_target: "h:1", media: "verbatim" })
    expect(mediaModeOf(stated)).toBe("verbatim")
    expect(mediaModeOf(config)).toBe("rebooked")
    expect(() =>
      decodeRunConfigSync({ lane: "upstream-fake", clock: "virtual", route_target: "h:1", media: "rewritten" })
    ).toThrow()
  })
})

describe("a recorded message", () => {
  it("states the leg as the FILE, never as a field", () => {
    expect(() => decodeRecordedMessageSync({ seq: 1, dir: "out", at_us: 0, raw: "X", leg: "A" })).toThrow()
  })

  it("omits the fields no step claimed rather than writing nulls", () => {
    const line = emitRecordedMessage(decodeRecordedMessageSync({ seq: 1, dir: "out", at_us: 0, raw: "X" }))
    expect(line).toBe('{"at_us":0,"dir":"out","raw":"X","seq":1}')
  })
})

describe("a run that never settled", () => {
  it("does not carry the field at all", () => {
    const unsettled = decodeRunTimingSync({ started_at_ms: 0, settle_budget_ms: 32_000 })
    expect(emitRunTiming(unsettled)).not.toContain("settled_at_ms")
  })
})

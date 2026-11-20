/**
 * The rule families: each row expands to the rules it names, in place, each
 * keyed as the family states and matching what its parameter names.
 */
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { carried, keepalive, strays, substitution } from "../src/families.js"
import { section } from "../src/rule.js"
import { headerProbe, shape } from "./probes.js"

type Facts = Record<string, never>

const NONE: Facts = {}

const text = (id: string) => ({ id, why: "" })

const onBye: Probe.MsgScope = { kind: "request", method: "BYE", inDialog: true }
const onReinvite: Probe.MsgScope = { kind: "request", method: "INVITE", inDialog: true }

describe("a carried header family", () => {
  const rules = section<Facts, "carried">("carried", carried(["subject"], {
    relayed: text("subject-not-relayed"),
    generated: text("subject-not-generated")
  }))
  const lost = (over: Partial<Probe.HeaderProbe>) =>
    headerProbe({ name: "Subject", scope: onBye, captured: ["lunch"], inbound: true, inboundValues: ["lunch"], ...over })
  const namedBy = (probe: Probe.Probe) => rules.filter((rule) => rule.matches(probe, NONE)).map((rule) => rule.id)

  it("expands to its relayed then its generated rule", () => {
    expect(rules.map((rule) => rule.id)).toEqual(["subject-not-relayed", "subject-not-generated"])
    expect(rules.map((rule) => rule.on)).toEqual([{ kind: "header", family: ["subject"] }, { kind: "header", family: ["subject"] }])
  })

  it("names a loss handed in as not relayed, and a loss the system minted as not generated", () => {
    expect(namedBy(lost({}))).toEqual(["subject-not-relayed"])
    expect(namedBy(lost({ inbound: false, inboundValues: [] }))).toEqual(["subject-not-generated"])
    expect(namedBy(lost({ driven: false }))).toEqual([])
    expect(namedBy(lost({ name: "Organization" }))).toEqual([])
    expect(namedBy(lost({ replayed: ["lunch"] }))).toEqual([])
  })

  it("expands only the rows it is given, held to an INVITE where it is so scoped", () => {
    const onInvite = section<Facts, "carried">("carried", carried(["subject"], { relayed: text("subject-on-invite") }, "INVITE"))
    expect(onInvite.map((rule) => rule.on)).toEqual([{ kind: "header", family: ["subject"], scope: ["INVITE"] }])
    expect(onInvite[0]!.matches(lost({ scope: onReinvite }), NONE)).toBe(true)
    expect(onInvite[0]!.matches(lost({}), NONE)).toBe(false)
  })
})

describe("the keepalive family", () => {
  const rules = section<Facts, "keepalive">("keepalive", keepalive<Facts>([
    { ...text("keepalive-subject"), family: ["subject"], also: (h) => h.replayed.length > 0 },
    text("keepalive-any")
  ]))
  const onOptions = (name: string, scope: Probe.MsgScope, replayed: ReadonlyArray<string>) =>
    headerProbe({ name, scope, captured: ["a"], replayed })

  it("cites RFC 3261 §11.2 and keys each row on OPTIONS, on its family where it names one", () => {
    expect(rules.map((rule) => [rule.rfc, rule.on])).toEqual([
      ["RFC 3261 §11.2", { kind: "header", family: ["subject"], scope: ["OPTIONS"] }],
      ["RFC 3261 §11.2", { kind: "header", scope: ["OPTIONS"] }]
    ])
  })

  it("names a header of the in-dialog OPTIONS exchange, request or answer, as its row holds", () => {
    const request: Probe.MsgScope = { kind: "request", method: "OPTIONS", inDialog: true }
    const answer: Probe.MsgScope = { kind: "response", status: 200, cseqMethod: "OPTIONS" }
    expect(rules[0]!.matches(onOptions("Subject", request, ["b"]), NONE)).toBe(true)
    expect(rules[0]!.matches(onOptions("Subject", request, []), NONE)).toBe(false)
    expect(rules[1]!.matches(onOptions("Accept", answer, []), NONE)).toBe(true)
    expect(rules[1]!.matches(onOptions("Accept", { kind: "request", method: "OPTIONS", inDialog: false }, []), NONE)).toBe(false)
  })
})

describe("the stray family", () => {
  const rules = section<Facts, "strays">("strays", strays<Facts>({ OPTIONS: text("options-stray"), INFO: text("info-stray") }))

  it("names one rule per method, each the stray of its own method", () => {
    const options = shape({ shape: "serviced-stray", method: "OPTIONS", action: "answered" })
    const info = shape({ shape: "extra-message", method: "INFO" })
    expect(rules.map((rule) => rule.id)).toEqual(["options-stray", "info-stray"])
    expect(rules.map((rule) => rule.matches(options, NONE))).toEqual([true, false])
    expect(rules.map((rule) => rule.matches(info, NONE))).toEqual([false, true])
    expect(rules.map((rule) => rule.matches(shape({ shape: "missing-message", method: "INFO" }), NONE))).toEqual([false, false])
  })
})

describe("a status substitution row", () => {
  const answered = (observed: number, scope: Probe.MsgScope) =>
    shape({ shape: "status-substitution", captured: 200, observed }, scope)

  it("keys on its CSeq method, or on INVITE where it is held to the initial INVITE", () => {
    const [onByeRow, initial, any] = section<Facts, "s">("s", [
      substitution<Facts>(text("bye-481"), { observed: 481, cseq: "BYE" }),
      substitution<Facts>(text("initial-480"), { observed: 480, at: "initial-invite" }),
      substitution<Facts>(text("any-503"), { observed: 503 })
    ])
    expect(onByeRow!.on).toEqual({ kind: "shape", family: ["status-substitution"], scope: ["BYE"] })
    expect(initial!.on).toEqual({ kind: "shape", family: ["status-substitution"], scope: ["INVITE"] })
    expect(any!.on).toEqual({ kind: "shape", family: ["status-substitution"] })
    expect(onByeRow!.matches(answered(481, { kind: "response", status: 481, cseqMethod: "BYE" }), NONE)).toBe(true)
    expect(onByeRow!.matches(answered(481, { kind: "response", status: 481, cseqMethod: "INFO" }), NONE)).toBe(false)
    expect(initial!.matches(answered(480, { kind: "initial-invite" }), NONE)).toBe(true)
    expect(any!.matches(answered(503, { kind: "response", status: 503, cseqMethod: "INFO" }), NONE)).toBe(true)
  })
})

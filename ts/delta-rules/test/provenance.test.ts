/**
 * Where a header's items came from: what the capture shows the system being
 * handed, whether the run drove the header in, and the relay, mint and loss
 * readings built on the two.
 */
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  allHanded,
  amongInboundValues,
  drivenIn,
  droppedFromAck,
  glareRejectionLost,
  inboundItems,
  lostWereHanded,
  notGenerated,
  notRelayed,
  relayInputHeld,
  undrivenLoss,
  unidentifiedLoss
} from "../src/provenance.js"
import { headerProbe } from "./probes.js"

const probe = (over: Partial<Probe.HeaderProbe>): Probe.HeaderProbe => headerProbe(over) as Probe.HeaderProbe

const onBye: Probe.MsgScope = { kind: "request", method: "BYE", inDialog: true }
const onAck: Probe.MsgScope = { kind: "request", method: "ACK", inDialog: true }

/** A Subject the capture carried and the replay lost, on a BYE the capture shows reaching the system. */
const lostSubject = (over: Partial<Probe.HeaderProbe>): Probe.HeaderProbe =>
  probe({ name: "Subject", scope: onBye, captured: ["lunch"], inbound: true, inboundValues: ["lunch"], ...over })

describe("what the capture shows the system being handed", () => {
  const supported = probe({ name: "Supported", inbound: true, inboundValues: ["100rel, timer"] })

  it("holds every inbound value whole and split as the header's grammar splits it", () => {
    expect([...inboundItems(supported)].sort()).toEqual(["100rel", "100rel, timer", "timer"])
  })

  it("holds items handed where each is an inbound item, and byte-equal where each is a whole value", () => {
    expect(allHanded(supported, [" timer "])).toBe(true)
    expect(allHanded(supported, ["timer", "path"])).toBe(false)
    expect(amongInboundValues(supported, ["100rel, timer"])).toBe(true)
    expect(amongInboundValues(supported, ["timer"])).toBe(false)
  })
})

describe("what the run drove in", () => {
  it("holds a header driven in, or one of a message the run identified no input for", () => {
    expect(drivenIn(probe({ driven: true }))).toBe(true)
    expect(drivenIn(probe({ driven: undefined }))).toBe(true)
    expect(drivenIn(probe({ driven: false }))).toBe(false)
  })

  it("licenses a relay claim on the initial INVITE only where the run drove the header in", () => {
    expect(relayInputHeld(probe({ driven: true }))).toBe(true)
    expect(relayInputHeld(probe({ driven: undefined }))).toBe(false)
    expect(relayInputHeld(probe({ scope: onBye, driven: undefined }))).toBe(true)
    expect(relayInputHeld(probe({ scope: onBye, driven: false }))).toBe(false)
  })
})

describe("what the replay lost", () => {
  it("holds a loss handed in where every lost item is one the capture shows the system being handed", () => {
    expect(lostWereHanded(lostSubject({}))).toBe(true)
    expect(lostWereHanded(lostSubject({ inboundValues: ["dinner"] }))).toBe(false)
    const narrowed = probe({ name: "Supported", captured: ["100rel, timer"], replayed: ["timer"], inboundValues: ["100rel"] })
    expect(lostWereHanded(narrowed)).toBe(true)
  })

  it("reads a loss of a header the run never drove in, though the capture shows it reaching the system", () => {
    expect(undrivenLoss(lostSubject({ driven: false }))).toBe(true)
    expect(undrivenLoss(lostSubject({ driven: undefined }))).toBe(false)
    expect(undrivenLoss(lostSubject({ driven: false, inbound: false }))).toBe(false)
    expect(undrivenLoss(lostSubject({ driven: false, replayed: ["lunch"] }))).toBe(false)
  })

  it("reads a loss on the initial INVITE the run identified no input for, there alone", () => {
    expect(unidentifiedLoss(lostSubject({ scope: { kind: "initial-invite" }, driven: undefined }))).toBe(true)
    expect(unidentifiedLoss(lostSubject({ driven: undefined }))).toBe(false)
    expect(unidentifiedLoss(lostSubject({ scope: { kind: "initial-invite" }, driven: true }))).toBe(false)
  })
})

describe("a header relayed or minted", () => {
  it("is not relayed where the capture shows it reaching the system, driven in, and the replay omits it", () => {
    expect(notRelayed(lostSubject({ driven: true }))).toBe(true)
    expect(notRelayed(lostSubject({ driven: false }))).toBe(false)
    expect(notRelayed(lostSubject({ inbound: false }))).toBe(false)
  })

  it("is not generated where the captured system minted it, never handed it, and the replay omits it", () => {
    expect(notGenerated(lostSubject({ inbound: false }))).toBe(true)
    expect(notGenerated(lostSubject({}))).toBe(false)
  })

  it("is dropped from an ACK where an ACK lost a header driven in", () => {
    expect(droppedFromAck(lostSubject({ scope: onAck }))).toBe(true)
    expect(droppedFromAck(lostSubject({}))).toBe(false)
    expect(droppedFromAck(lostSubject({ scope: onAck, driven: false }))).toBe(false)
  })

  it("is lost from a glare rejection where the replay's 491 to INVITE lacks what the capture's carried", () => {
    const on491 = (cseqMethod: string): Probe.MsgScope => ({ kind: "response", status: 491, cseqMethod })
    expect(glareRejectionLost(probe({ name: "Retry-After", scope: on491("INVITE"), captured: ["5"] }))).toBe(true)
    expect(glareRejectionLost(probe({ name: "Retry-After", scope: on491("UPDATE"), captured: ["5"] }))).toBe(false)
    expect(glareRejectionLost(probe({ name: "Retry-After", scope: on491("INVITE"), captured: ["5"], replayed: ["3"] }))).toBe(false)
  })
})

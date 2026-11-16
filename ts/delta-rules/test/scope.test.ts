/**
 * Where a probe sits: the request or response it rides, its method and status,
 * whether the request refreshes the dialog target, and the role of its leg.
 */
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { inDialogRequest, isRequest, isTargetRefresh, onAnInvite, onKeepalive, onLeg, statusOf } from "../src/scope.js"
import { headerProbe } from "./probes.js"

const request = (method: string, inDialog = true): Probe.MsgScope => ({ kind: "request", method, inDialog })
const response = (status: number, cseqMethod: string): Probe.MsgScope => ({ kind: "response", status, cseqMethod })
const INITIAL: Probe.MsgScope = { kind: "initial-invite" }

describe("the message a probe rides", () => {
  it("states a response's status, to a CSeq method where one is named", () => {
    expect(statusOf(response(180, "INVITE"))).toBe(180)
    expect(statusOf(response(200, "BYE"), "BYE")).toBe(200)
    expect(statusOf(response(200, "BYE"), "INVITE")).toBeUndefined()
    expect(statusOf(request("BYE"))).toBeUndefined()
    expect(statusOf(undefined)).toBeUndefined()
  })

  it("reads a request, of a method where one is named, the method as written (RFC 3261 §7.1)", () => {
    expect(isRequest(request("BYE"))).toBe(true)
    expect(isRequest(request("BYE"), "BYE")).toBe(true)
    expect(isRequest(request("bye"), "BYE")).toBe(false)
    expect(isRequest(response(200, "BYE"), "BYE")).toBe(false)
    expect(isRequest(INITIAL)).toBe(false)
  })

  it("reads an in-dialog request apart from one outside a dialog", () => {
    expect(inDialogRequest(request("OPTIONS"), "OPTIONS")).toBe(true)
    expect(inDialogRequest(request("OPTIONS", false), "OPTIONS")).toBe(false)
    expect(inDialogRequest(request("INFO"))).toBe(true)
  })

  it("reads the initial INVITE and a re-INVITE as an INVITE, never its response", () => {
    expect(onAnInvite(INITIAL)).toBe(true)
    expect(onAnInvite(request("INVITE"))).toBe(true)
    expect(onAnInvite(response(200, "INVITE"))).toBe(false)
    expect(onAnInvite(request("UPDATE"))).toBe(false)
  })

  it("reads the in-dialog OPTIONS exchange, request or answer", () => {
    expect(onKeepalive(request("OPTIONS"))).toBe(true)
    expect(onKeepalive(response(200, "OPTIONS"))).toBe(true)
    expect(onKeepalive(request("OPTIONS", false))).toBe(false)
    expect(onKeepalive(response(200, "INVITE"))).toBe(false)
  })
})

describe("a target refresh", () => {
  it.each(["INVITE", "UPDATE", "SUBSCRIBE", "NOTIFY", "REFER", "REGISTER"])("counts %s", (method) => {
    expect(isTargetRefresh(method)).toBe(true)
  })

  it.each(["ACK", "BYE", "CANCEL", "PRACK", "INFO", "MESSAGE", "OPTIONS"])("never counts %s", (method) => {
    expect(isTargetRefresh(method)).toBe(false)
  })
})

describe("the leg a probe was observed on", () => {
  it("names a probe of a leg whose role is listed, never a probe with no leg", () => {
    const onCaller = headerProbe({ leg: { role: "caller", direction: "to-caller" } })
    expect(onLeg(onCaller, ["caller"])).toBe(true)
    expect(onLeg(onCaller, ["fork"])).toBe(false)
    expect(onLeg(headerProbe({}), ["caller"])).toBe(false)
  })
})

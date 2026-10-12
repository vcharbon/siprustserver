/**
 * The triage launch line a view copies: the deployment's command, the
 * capture, the one call where the view is of one, and the lane it was read
 * on — so a line copied off one call of a big capture hands that call over
 * and not the capture around it.
 */
import { describe, expect, it } from "vitest"
import { launchRef } from "../src/launch.ts"

const TRIAGE = "triage"

describe("launchRef", () => {
  it("names the capture alone by default", () => {
    expect(launchRef(TRIAGE, "c.pcap")).toBe("triage c.pcap")
    expect(launchRef(TRIAGE, "c.pcap", "")).toBe("triage c.pcap")
  })

  it("carries the lane the view was read on", () => {
    expect(launchRef(TRIAGE, "c.pcap", "fake")).toBe("triage c.pcap --lane fake")
  })

  it("hands over the one call before the lane", () => {
    expect(launchRef(TRIAGE, "c.pcap", "fake", "c-case1")).toBe("triage c.pcap --case c-case1 --lane fake")
    expect(launchRef(TRIAGE, "c.pcap", undefined, "c-case1")).toBe("triage c.pcap --case c-case1")
    expect(launchRef(TRIAGE, "c.pcap", "fake", "")).toBe("triage c.pcap --lane fake")
  })
})

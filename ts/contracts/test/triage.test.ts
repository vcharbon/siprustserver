/**
 * The triage-registry mirror, against the sample the format was designed on.
 *
 * The registry is the only contract here Rust does not own; what it must hold is
 * the invariant the checker rests on — a PENDING verdict carries a ticket, a
 * FINAL one does not need to — so that is what is pinned.
 */
import { describe, expect, it } from "vitest"
import { decodeRegistrySync, disposition, isFinal, ticketStateOfText } from "../src/triage.js"
import { LOCAL_FIXTURES, read } from "./fixtures.js"

const registry = decodeRegistrySync(JSON.parse(read(LOCAL_FIXTURES, "registry.sample.json")) as unknown)

describe("the sample registry", () => {
  it("is keyed by the corpus capture basename", () => {
    for (const key of Object.keys(registry)) expect(key.endsWith(".pcap.gz")).toBe(true)
  })

  it("holds one record of each verdict", () => {
    expect(Object.values(registry).map((r) => r.verdict).sort()).toEqual(["bug", "cut", "expected", "never-fix"])
  })

  it("makes the verdict alone decide finality", () => {
    for (const record of Object.values(registry)) {
      if (isFinal(record)) expect(["expected", "never-fix"]).toContain(record.verdict)
      else expect(record.ticket.length).toBeGreaterThan(0)
    }
  })

  it("narrows a record to specific cut cases only where it says so", () => {
    const narrowed = Object.values(registry).filter((r) => r.cases !== undefined)
    expect(narrowed.length).toBe(1)
    expect(narrowed[0].verdict).toBe("cut")
  })
})

describe("the ticket-state read", () => {
  it("reads the leading token of the Status: line, case-insensitively, ignoring trailing notes", () => {
    expect(ticketStateOfText("# t\n\nStatus: needs-triage\n")).toBe("open")
    expect(ticketStateOfText("Status: claimed")).toBe("open")
    expect(ticketStateOfText("Status: resolved (2026-08-25)")).toBe("closed")
    expect(ticketStateOfText("Status: DONE 2026-08-24 (638cac6)")).toBe("closed")
    expect(ticketStateOfText("Status: wontfix — superseded")).toBe("wontfix")
  })

  it("treats a missing file as missing and a file with no Status line as open", () => {
    expect(ticketStateOfText(undefined)).toBe("missing")
    expect(ticketStateOfText("# a ticket with no state yet")).toBe("open")
  })
})

describe("the disposition checker", () => {
  const pending = Object.values(registry).find((record) => !isFinal(record))!
  const final = Object.values(registry).find(isFinal)!

  it("takes an unrecorded capture", () => {
    expect(disposition(undefined, undefined)).toBe("untriaged")
  })

  it("excludes a final record whatever its ticket says", () => {
    expect(disposition(final, "open")).toBe("excluded")
    expect(disposition(final, undefined)).toBe("excluded")
  })

  it("suppresses a pending record while its ticket is open or wontfix", () => {
    expect(disposition(pending, "open")).toBe("suppressed")
    expect(disposition(pending, "wontfix")).toBe("suppressed")
  })

  it("re-queues a pending record once its ticket closes or vanishes", () => {
    expect(disposition(pending, "closed")).toBe("requeue")
    expect(disposition(pending, "missing")).toBe("requeue")
  })
})

const EVIDENCE = { bundle: "corpus-work/triage/x/run1", note: "n" }

describe("strictness", () => {
  it("refuses a pending verdict with no ticket", () => {
    expect(() =>
      decodeRegistrySync({ "x.pcap.gz": { verdict: "bug", decided_on: "2026-08-25", evidence: EVIDENCE } })
    ).toThrow()
  })

  it("refuses a final verdict that carries a ticket it does not need", () => {
    expect(() =>
      decodeRegistrySync({
        "x.pcap.gz": { verdict: "expected", ticket: "t.md", decided_on: "2026-08-25", evidence: EVIDENCE }
      })
    ).toThrow()
  })

  it("refuses a verdict outside the five", () => {
    expect(() =>
      decodeRegistrySync({ "x.pcap.gz": { verdict: "maybe", decided_on: "2026-08-25", evidence: EVIDENCE } })
    ).toThrow()
  })

  it("refuses a record with no evidence at all", () => {
    expect(() => decodeRegistrySync({ "x.pcap.gz": { verdict: "expected", decided_on: "2026-08-25" } })).toThrow()
  })
})

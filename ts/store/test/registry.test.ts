/**
 * The triage registry, against the sample the
 * format was designed on (`fixtures/registry.sample.json`).
 *
 * No Rust model owns the registry; what it must hold is the invariant the
 * checker rests on — a PENDING verdict carries a ticket, a FINAL one does not
 * need to — so that is what is pinned.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { decodeRegistrySync, disposition, isFinal } from "../src/registry.js"

const SAMPLE = path.join(import.meta.dirname, "fixtures", "registry.sample.json")
const registry = decodeRegistrySync(JSON.parse(fs.readFileSync(SAMPLE, "utf8")) as unknown)

describe("the sample registry", () => {
  it("makes the verdict alone decide finality", () => {
    for (const record of Object.values(registry)) {
      if (isFinal(record)) expect(["expected", "never-fix"]).toContain(record.verdict)
      else expect(record.ticket.length).toBeGreaterThan(0)
    }
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

const EVIDENCE = { bundle: "work/triage/x/run1", note: "n" }

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

/**
 * What a ticket's text cites of the store: runs, jobs, sweeps, preps and case
 * ids, each dated by when the text that cites it was written; case ids read by
 * the deployment's grammar.
 */
import { describe, expect, it } from "vitest"
import { Resolver } from "@sip/store"
import { citationsOf, type TicketText } from "../../src/ids/citations.js"

const ticket = (texts: ReadonlyArray<{ readonly text: string; readonly on: string }>): TicketText => ({
  ref: "proj/7",
  texts
})

/** The scan under the store's default grammar. */
const scan = (one: TicketText) => citationsOf(one, Resolver.DEFAULT_GRAMMAR)

const refsOf = (found: ReturnType<typeof citationsOf>, kind: string) =>
  found.filter((one) => one.kind === kind).map((one) => one.ref)

describe("citationsOf", () => {
  it("reads every run, job, sweep and prep a text names, in each spelling, once", () => {
    const found = scan(ticket([{
      text: "Seen on run100201 and run 100202 (sweep 301, job 42), `@run:7`, run9105/ held 886 cells;" +
        " prep60 and prep 71 cut it; run100201 again",
      on: "2026-10-11T19:00:00.000Z"
    }]))
    expect(refsOf(found, "run")).toEqual(["100201", "100202", "7", "9105"])
    expect(refsOf(found, "job")).toEqual(["42"])
    expect(refsOf(found, "sweep")).toEqual(["301"])
    expect(refsOf(found, "prep")).toEqual(["60", "71"])
    expect(found.every((one) => one.citedBy === "proj/7" && one.citedOn === "2026-10-11")).toBe(true)
  })

  it("reads every case id form, the run-scoped one included, and a bare suffix", () => {
    const found = scan(ticket([{
      text: "`traceX.anon-case17` refused; capture_ab12cd34-0001.pcap.gz-auto, " +
        "Q100.pcap-cut2, run200300/case1, A1.02_x_sip.pcapng.gz-legs3+4, " +
        "and case437 on its own",
      on: "2026-10-01T00:00:00.000Z"
    }]))
    expect([...refsOf(found, "case")].sort()).toEqual([
      "A1.02_x_sip.pcapng.gz-legs3+4",
      "Q100.pcap-cut2",
      "capture_ab12cd34-0001.pcap.gz-auto",
      "case437",
      "run200300/case1",
      "traceX.anon-case17"
    ])
  })

  it("reads a case id ending in a suffix the deployment's grammar adds, and none it does not", () => {
    const text = { text: "a.anon2-x0123 and b.pcap-y9", on: "2026-10-01T00:00:00.000Z" }
    const grammar = { isCapture: () => false, suffixes: String.raw`case\d+|x[0-9a-f]{4}` }
    expect(refsOf(citationsOf(ticket([text]), grammar), "case")).toEqual(["a.anon2-x0123"])
    expect(refsOf(scan(ticket([text])), "case")).toEqual([])
  })

  it("dates each citation by the text that holds it, the earliest when several do", () => {
    const found = scan(ticket([
      { text: "run 9 is red", on: "2026-09-25T10:00:00.000Z" },
      { text: "still run 9", on: "2026-09-10T10:00:00.000Z" }
    ]))
    expect(found).toEqual([{ kind: "run", ref: "9", citedBy: "proj/7", citedOn: "2026-09-10" }])
  })

  it("reads plural, listed and capitalised forms, each number of a list on its own", () => {
    const found = scan(ticket([{
      text: "Runs 4063, 5400 and 5976 failed; runs 5308/6099 & 6104 too. Run 12 then " +
        "Sweep 136, Sweeps 95, 50 and 172-173, jobs 41 and 42; Preps 60, 71; JOB #7",
      on: "2026-10-11T19:00:00.000Z"
    }]))
    expect(refsOf(found, "run")).toEqual(["4063", "5400", "5976", "5308", "6099", "6104", "12"])
    expect(refsOf(found, "sweep")).toEqual(["136", "95", "50", "172", "173"])
    expect(refsOf(found, "job")).toEqual(["41", "42", "7"])
    expect(refsOf(found, "prep")).toEqual(["60", "71"])
  })

  it("reads arrows, `from … to`, `of`, table pipes and a parenthesised list", () => {
    const found = scan(ticket([{
      text: "sweep 148 → 151; prep from 64 to 66; the sweeps of 113/114 (173, 175); sweep | 163 | 164 |; " +
        "Sweep **139 vs 138**, sweep `122`, sweep 52 against 50",
      on: "2026-10-11T19:00:00.000Z"
    }]))
    expect(refsOf(found, "sweep")).toEqual(
      ["148", "151", "113", "114", "173", "175", "163", "164", "139", "138", "122", "52", "50"]
    )
    expect(refsOf(found, "prep")).toEqual(["64", "65", "66"])
  })

  it("expands a short range and keeps only the ends of a long one", () => {
    const found = scan(ticket([{
      text: "sweeps 199–202 and runs 1..300000; jobs from 43 through 45",
      on: "2026-10-11T19:00:00.000Z"
    }]))
    expect(refsOf(found, "sweep")).toEqual(["199", "200", "201", "202"])
    expect(refsOf(found, "job")).toEqual(["43", "44", "45"])
    expect(refsOf(found, "run")).toEqual(["1", "300000"])
  })

  it("reads no run in a word that only contains one", () => {
    expect(scan(ticket([{ text: "rerun 5 times, runner3, a dry-run 2", on: "2026-10-01T00:00:00.000Z" }])))
      .toEqual([])
  })
})

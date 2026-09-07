/**
 * The `sipflow` adapter. Two things are its own and are pinned here: the argv
 * it builds — the `--emit-headers` allow-list must REACH the emitter, or every
 * rule that reads a header matches nothing — and that what comes back is
 * decoded rather than handed on as text.
 */
import { SipflowCli } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { fixture, rig } from "./harness.js"

const STUB = { SIPFLOW: fixture("stub-sipflow.sh") }

const exec = <A, E>(use: (cli: SipflowCli.Interface) => Effect.Effect<A, E>) =>
  Effect.runPromiseExit(
    Effect.gen(function* () {
      return yield* use(yield* SipflowCli.Service)
    }).pipe(Effect.provide(rig(SipflowCli.layer, STUB)))
  )

const value = async <A, E>(use: (cli: SipflowCli.Interface) => Effect.Effect<A, E>): Promise<A> => {
  const exit = await exec(use)
  if (exit._tag !== "Success") throw new Error(`failed: ${JSON.stringify(exit)}`)
  return exit.value
}

describe("flows", () => {
  it("decodes the document rather than handing back its text", async () => {
    const doc = await value((cli) => cli.flows("capture.pcap.gz", ["P-Charging-Vector"]))
    expect(doc.schema).toBe(5)
    expect(doc.legs[0].call_id).toBe("c1")
    expect(doc.legs[0].msgs[0].summary.kind).toBe("request")
  })

  it("hands the emitter the allow-list the caller asked for, comma-joined", async () => {
    // The stub echoes the `--emit-headers` argv back as the document's own
    // `emit_headers`, so the decoded document IS the argv assertion.
    const doc = await value((cli) => cli.flows("capture.pcap.gz", ["P-Charging-Vector", "X-Api-Call"]))
    expect(doc.emit_headers).toEqual(["P-Charging-Vector", "X-Api-Call"])
  })

  it("asks for no allow-list when the caller names no header", async () => {
    const doc = await value((cli) => cli.flows("capture.pcap.gz", []))
    expect(doc.emit_headers).toEqual([])
  })

  it("fails when the capture is not there", async () => {
    const exit = await exec((cli) => cli.flows("missing.pcap", []))
    expect(exit._tag).toBe("Failure")
    expect(JSON.stringify(exit)).toContain("Toolchain.CliFailed")
  })
})

describe("schema", () => {
  it("reads the flows document's own JSON Schema", async () => {
    expect(JSON.parse(await value((cli) => cli.schema()))).toEqual({ title: "FlowsDoc" })
  })
})

describe("rfcCensus", () => {
  it("hands back the report raw — its shape has no mirror yet — over a comma-joined path list", async () => {
    const report = JSON.parse(await value((cli) => cli.rfcCensus(["a.flows.json", "b.flows.json"])))
    expect(report).toEqual({ documents: 0, argv: "a.flows.json,b.flows.json" })
  })
})

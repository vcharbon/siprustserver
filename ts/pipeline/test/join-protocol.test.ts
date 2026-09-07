/**
 * A join reading that names the protocol it fired on states it as a case flag:
 * the document freezes only the peer's own replies, so a policy reading the
 * layout is the deployment's one source for what the joined dialog spoke.
 */
import { describe, expect, it } from "vitest"
import { build, JOIN_PROTOCOL_FLAG, type JoinsReading } from "../src/topology.js"
import type { Vantage } from "../src/selection.js"
import { cancelRaceFlows, derivesOnePrefix, plan, sutSet } from "./fixtures.js"

const BOTH_VANTAGES: ReadonlyArray<Vantage> = [
  { leg: 0, hop: 0 },
  { leg: 1, hop: 0 }
]

const PROTOCOL = "application/mediaservercontrol+xml"

const layoutWith = (reading: JoinsReading) =>
  build(cancelRaceFlows(), BOTH_VANTAGES, sutSet(), plan(), derivesOnePrefix, [], () => reading)

/** The called leg, read as a media resource speaking the current protocol. */
const joined = (protocol?: string): JoinsReading =>
  new Map([[1, { kind: "mrf" as const, ...(protocol ? { protocol } : {}), evidence: ["stub"] }]])

describe("the protocol a join was read by", () => {
  it("lands on the case as a flag naming the leg it was read on", () => {
    const flags = layoutWith(joined(PROTOCOL)).flags

    expect(flags.map((f) => f.kind)).toContain(`${JOIN_PROTOCOL_FLAG}${PROTOCOL}`)
    expect(flags.find((f) => f.kind.startsWith(JOIN_PROTOCOL_FLAG))!.detail).toBe(
      `leg B: the mrf join was read off a ${PROTOCOL} channel`
    )
  })

  it("states nothing for a reading that named no protocol", () => {
    expect(layoutWith(joined()).flags.filter((f) => f.kind.startsWith(JOIN_PROTOCOL_FLAG))).toEqual(
      []
    )
  })

  it("states nothing where no leg was joined", () => {
    expect(layoutWith(new Map()).flags.filter((f) => f.kind.startsWith(JOIN_PROTOCOL_FLAG))).toEqual(
      []
    )
  })
})

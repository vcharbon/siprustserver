/** Minimal v3 documents built by hand, so a layout test states exactly one thing. */
import type { Delay, FlowNode, PivotV3, Step } from "../src/generated/pivot-v3.ts"

export const delay = (ms = 0, from = "trigger"): Delay => ({
  ms,
  from,
  compressible: true,
  timer_linked: false
})

export const send = (id: string, leg: string, method: string, extra: Partial<Step> = {}): Step => ({
  id,
  leg,
  op: "send",
  msg: { method },
  delay: delay(),
  ...extra
})

export const expectResponse = (
  id: string,
  leg: string,
  status: number,
  reason: string,
  extra: Partial<Step> = {}
): Step => ({
  id,
  leg,
  op: "expect",
  check: "assert",
  msg: { status, reason, "cseq-method": "INVITE" },
  delay: delay(),
  ...extra
})

export const expectRequest = (id: string, leg: string, method: string, extra: Partial<Step> = {}): Step => ({
  id,
  leg,
  op: "expect",
  check: "assert",
  msg: { method },
  delay: delay(),
  ...extra
})

export const document = (flow: readonly FlowNode[], overrides: Partial<PivotV3> = {}): PivotV3 => ({
  pivot_version: 3,
  case: {
    id: "unit",
    title: "unit case",
    family: "transparent",
    variant: "repro",
    origin: "authored",
    lanes: { fake: "ok" }
  },
  identities: [{ name: "called-0-0", kind: "site", observed: "+15550100004" }],
  calls: [{
    id: "c1",
    caller_leg: "A",
    attempts: [{ branch: 0, position: 0, leg: "B", callee: { identity: "called-0-0" } }]
  }],
  endpoints: [
    { id: "ep0", observed: "127.0.0.1:5060", side: "peer", binding: "dedicated" },
    { id: "ep1", observed: "127.0.0.1:5070", side: "peer", binding: "dedicated" }
  ],
  actors: [
    { id: "uac1", kind: "uac", endpoint: "ep0" },
    { id: "uas1", kind: "uas", endpoint: "ep1" }
  ],
  legs: [
    { id: "A", actor: "uac1", dir: "out" },
    { id: "B", actor: "uas1", dir: "in" }
  ],
  flow: [...flow],
  timing: { expect_budget_ms: 32000, settle_budget_ms: 32000 },
  ...overrides
})

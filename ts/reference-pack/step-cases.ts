/**
 * The reference pack's case source for an operation as a step of a call
 * (ADR-0060 appendix §7), named on its own (`--sources`), since its tag must
 * be one the run's workspace declares running alone. Alice calls Bob, who
 * answers; once Bob received the ACK, the lane's `restart-worker` kills the
 * system under test, which forgets the call: each party's BYE is answered
 * 481 by the system stood again, and no call record is written. Alice then
 * calls Bob again, through the system the heal left, as in the pack's own
 * `alice calls bob`. Both carry the tag `restarts`, so the second runs after
 * the first has healed.
 */
import { defineBasicCase } from "@sip/basic-case"

/** The tag both cases carry: a workspace running them declares it `alone`. */
export const TAG = "restarts"

const PARTIES = { caller: "sip:alice@atlanta.example.com", callee: "sip:bob@biloxi.example.com" } as const

export default [
  defineBasicCase({
    title: "alice calls bob through a restart",
    shape: "basic_call",
    ...PARTIES,
    final: [{ field: "status", eq: 200 }],
    tags: [TAG],
    steps: [{ operation: "restart-worker", at: "answered", forgets: true }],
    claims: { "call-records": { count: 0 } }
  }),
  defineBasicCase({
    title: "alice calls bob after the restart",
    shape: "basic_call",
    ...PARTIES,
    final: [{ field: "status", eq: 200 }],
    tags: [TAG],
    claims: {
      "call-records": {
        count: 1,
        checks: [
          { field: "b_legs.0.disposition", op: "eq", value: "bridged" },
          { field: "termination.cause", op: "eq", value: "remote_bye" }
        ]
      }
    }
  })
]

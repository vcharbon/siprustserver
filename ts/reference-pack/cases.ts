/**
 * The reference pack's case source: two calls between the RFC 3261 example
 * parties in the minimal case form (`@sip/basic-case`), one per shape. Alice
 * calls Bob, who answers, and Alice hangs up; Alice calls Bob, who is busy
 * (486). Each claims the one call record the system writes of its call, and
 * how that record states the callee's answer and the call's end.
 */
import { defineBasicCase } from "@sip/basic-case"

export default [
  defineBasicCase({
    title: "alice calls bob",
    shape: "basic_call",
    caller: "sip:alice@atlanta.example.com",
    callee: "sip:bob@biloxi.example.com",
    final: [{ field: "status", eq: 200 }],
    claims: {
      "call-records": {
        count: 1,
        checks: [
          { field: "b_legs.0.disposition", op: "eq", value: "bridged" },
          { field: "termination.cause", op: "eq", value: "remote_bye" }
        ]
      }
    }
  }),
  defineBasicCase({
    title: "bob is busy",
    shape: "invite_reject",
    params: { status: 486 },
    caller: "sip:alice@atlanta.example.com",
    callee: "sip:bob@biloxi.example.com",
    final: [{ field: "status", eq: 486 }],
    claims: {
      "call-records": {
        count: 1,
        checks: [
          { field: "b_legs.0.disposition", op: "eq", value: "rejected" },
          { field: "termination.cause", op: "eq", value: "remote_final" }
        ]
      }
    }
  })
]

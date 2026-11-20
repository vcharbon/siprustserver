/**
 * The two cases of the minimal form (ADR-0060 appendix §17): a case source
 * yielding `alice calls bob` (answered, the caller hangs up) and `bob is busy`
 * (rejected 486), between the RFC 3261 example parties.
 */
import { defineBasicCase } from "../../src/index.js"

export default [
  defineBasicCase({
    title: "alice calls bob",
    shape: "basic_call",
    caller: "sip:alice@atlanta.example.com",
    callee: "sip:bob@biloxi.example.com",
    final: [{ field: "status", eq: 200 }],
    claims: { "call-records": { count: 1 } }
  }),
  defineBasicCase({
    title: "bob is busy",
    shape: "invite_reject",
    params: { status: 486 },
    caller: "sip:alice@atlanta.example.com",
    callee: "sip:bob@biloxi.example.com",
    final: [{ field: "status", eq: 486 }],
    claims: { "call-records": { count: 1 } }
  })
]

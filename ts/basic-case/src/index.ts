/**
 * `@sip/basic-case`: the minimal case form (ADR-0060 appendix §14).
 * `defineBasicCase` states one call between two plain SIP addresses through
 * the system under test, in one of two shapes, `basic_call` and
 * `invite_reject`, and compiles it to the cell a case source yields
 * (`@sip/rig` `CaseSource`).
 */
export { Address, BasicCase, DEFAULT_REJECT, type Ender, FinalCheck, idOf, REJECTS, Refused, type RejectStatus } from "./case.js"
export { compile, defineBasicCase, documentOf } from "./compile.js"
export { CallFlow } from "./flow.js"

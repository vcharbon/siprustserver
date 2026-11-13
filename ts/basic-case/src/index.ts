/**
 * `@sip/basic-case`: the minimal case form (ADR-0060 appendix §14).
 * `defineBasicCase` states one call between two plain SIP addresses through
 * the system under test, in one of two shapes, `basic_call` and
 * `invite_reject`, and compiles it to the cell a case source yields
 * (`@sip/rig` `CaseSource`). A call no shape plays is written on the step
 * builder (`CallFlow`) and made a document by `callDocumentOf`, the parties,
 * endpoints and session descriptions the form's.
 */
export { Address, BasicCase, DEFAULT_REJECT, type Ender, FinalCheck, idOf, REJECTS, Refused, type RejectStatus } from "./case.js"
export { type Call, callDocumentOf, compile, defineBasicCase, documentOf } from "./compile.js"
export { CallFlow } from "./flow.js"
export { ANSWER, OFFER, RESOURCES, REWRITE } from "./media.js"

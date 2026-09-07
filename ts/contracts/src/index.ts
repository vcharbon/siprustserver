/**
 * **@sip/contracts** — the TypeScript mirrors of the Rust-owned wire formats.
 *
 * Rust is the source of truth. Every schema here mirrors a serde struct in this
 * repository and carries no logic beyond decoding, encoding and canonical
 * formatting; a contract question is answered by the Rust file the module doc
 * names, never by this package.
 *
 * The mirrors ship as namespaces because the formats deliberately reuse names —
 * a pivot `Identity` is a registry entry, a flows `Identity` is a parsed URI —
 * and flattening them would make one shadow the other.
 */
export * as AllowedErrors from "./allowed-errors.js"
export * as Body from "./body.js"
export * as Bundle from "./bundle/index.js"
export * as Call from "./call.js"
export * as Campaign from "./campaign.js"
export * as Canonical from "./canonical.js"
export * as Case from "./case.js"
export * as CellHits from "./cell-hits.js"
export * as Census from "./census.js"
export * as Check from "./check.js"
export * as Confrontation from "./confrontation.js"
export * as Deviation from "./deviation.js"
export * as Differential from "./differential.js"
export * as E2e from "./e2e.js"
export * as Flow from "./flow.js"
export * as Flows from "./flows.js"
export * as Identity from "./identity.js"
export * as Lint from "./lint.js"
export * as LoadRun from "./loadrun.js"
export * as Msg from "./msg.js"
export * as MustFail from "./must-fail.js"
export * as Pivot from "./pivot.js"
export * as Placement from "./placement.js"
export * as Postcondition from "./postcondition.js"
export * as Rules from "./rules.js"
export * as Schedules from "./schedules.js"
export * as Seq from "./seq.js"
export { STRICT } from "./strict.js"
export * as Tokens from "./tokens.js"
export * as Triage from "./triage.js"
export * as Violation from "./violation.js"

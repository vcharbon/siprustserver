/**
 * **@sip/pipeline** — the generic extraction engine.
 *
 * A capture becomes cases in four moves: correlate the calls (`./engine.ts`),
 * cut which legs are one call of the system under test (`./cut.ts`), infer the
 * layout and synthesize the flow (`./topology.ts`, `./flowsteps.ts`), and
 * assemble the documents (`./assemble.ts`, `./caseset.ts`).
 *
 * Nothing here knows whose platform produced the capture. Every reading that
 * would need to — a derived Call-ID, a handover cause, a family label, a lane
 * verdict, a declared failure, a refusal — arrives as a {@link CasePolicy},
 * bound once through {@link LogicExtractor}:
 *
 * ```ts
 * import { CaseAssembler, LogicExtractor } from "@sip/pipeline"
 * import { Layer } from "effect"
 *
 * const pipeline = CaseAssembler.layer.pipe(Layer.provide(LogicExtractor.layerWith(myPolicy)))
 * ```
 *
 * The modules that hold no policy are exported as namespaces, because the
 * formats deliberately reuse names — a pipeline `Call` is a correlation unit, a
 * contracts `Call` is a document block — and flattening them would make one
 * shadow the other.
 */
export * as Assemble from "./assemble.js"
export * as Background from "./background.js"
export * as Bodies from "./bodies.js"
export * as Call from "./call.js"
export { CaseAssembler } from "./case-assembler.js"
export * as CaseSet from "./caseset.js"
export * as CaseSpec from "./case-spec.js"
export * as Calls from "./calls.js"
export * as CaptureIndex from "./capture-index.js"
export * as CaptureRules from "./capture-rules.js"
export * as CaseRules from "./case-rules.js"
export * as Census from "./census.js"
export { Classifier } from "./classifier.js"
export * as Confront from "./confront.js"
export * as Cut from "./cut.js"
export * as Delay from "./delay.js"
export * as Derivation from "./derivation.js"
export * as DocumentRules from "./document-rules.js"
export * as Draft from "./draft.js"
export * as Engine from "./engine.js"
export * as FarSideReinvite from "./far-side-reinvite.js"
export * as Finals from "./finals.js"
export * as FlowSteps from "./flowsteps.js"
export * as Fold from "./fold.js"
export * as Forms from "./forms.js"
export { LogicExtractor } from "./logic-extractor.js"
export * as MsgSpec from "./msgspec.js"
export * as Parts from "./parts.js"
export * as Plan from "./plan.js"
export * as Policy from "./policy.js"
export * as Probe from "./probe.js"
export * as Profile from "./profile.js"
export { Reclassifier } from "./reclassifier.js"
export * as RefusalRoster from "./refusal-roster.js"
export * as RefusalRule from "./refusal-rule.js"
export * as Runnable from "./runnable.js"
export * as Selection from "./selection.js"
export * as Sut from "./sut.js"
export * as Topology from "./topology.js"
export * as Transactions from "./transactions.js"
export * as Violations from "./violations.js"
export * as Wire from "./wire.js"

export { neutralPolicy, policyWith, UNCLASSIFIED, type CasePolicy } from "./policy.js"

/**
 * **@sip/delta-rules** — the engine a deployment writes its delta rules on
 * (ADR-0062). A replayed cell is confronted with its reference capture and
 * every difference is a probe; a delta rule names a probe and states why that
 * difference is not evidence about the system under test.
 *
 * | module | what it holds |
 * |---|---|
 * | `rule.ts`, `rule-on.ts` | the rule type, its match key, `section()`, first match |
 * | `families.ts` | the family builders: rules that differ by one parameter |
 * | `header-items.ts`, `provenance.ts`, `scope.ts`, `identity.ts`, `value-forms.ts`, `shapes.ts` | the readings a predicate is written with |
 * | `run-facts.ts`, `document-reading.ts` | what a gate reads off the run and the document |
 * | `lane-lists.ts` | the lane lists, their loader and the classification they decide |
 * | `overlap.ts` | the overlap graph and its golden form, which pin every winner |
 * | `rules-table.ts` | the deployment's rules page |
 *
 * The deployment supplies its case facts, its sections and rows, its lane
 * lists, its fixtures and the prose its page prints.
 */
export * as DocumentReading from "./document-reading.js"
export * as Families from "./families.js"
export * as HeaderItems from "./header-items.js"
export * as Identity from "./identity.js"
export * as LaneLists from "./lane-lists.js"
export * as Overlap from "./overlap.js"
export * as Provenance from "./provenance.js"
export * as Rule from "./rule.js"
export * as RuleOn from "./rule-on.js"
export * as RulesTable from "./rules-table.js"
export * as RunFacts from "./run-facts.js"
export * as Scope from "./scope.js"
export * as Shapes from "./shapes.js"
export * as ValueForms from "./value-forms.js"

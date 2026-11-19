/**
 * **@sip/rfc-delta-rules** — the generic delta rows (ADR-0062 item 5), written
 * on `@sip/delta-rules`: each names a difference in RFC or 3GPP terms, or the
 * documented behaviour of this repository's B2BUA or replay rig, and cites its
 * clause or ADR.
 *
 * | module | what it holds |
 * |---|---|
 * | `sections.ts` | the sections and the rows' match order, which `test/overlap.golden.json` pins |
 * | `keepalive.ts`, `stack-owned.ts`, `sender-owned.ts`, `capability.ts`, `captured-only.ts`, `shape.ts` | the rows, one module per section |
 * | `header-lists.ts` | the header lists the rows name a family by |
 * | `facts.ts` | the case facts the rows read, and how each is read off a cell |
 * | `row.ts` | the row type |
 *
 * The deployment places each row by name in its own sections, in this
 * package's order, adds its lane notes, and extends {@link Facts.RfcFacts}
 * with its own case facts.
 */
export * as Capability from "./capability.js"
export * as CapturedOnly from "./captured-only.js"
export * as Facts from "./facts.js"
export * as HeaderLists from "./header-lists.js"
export * as Keepalive from "./keepalive.js"
export * as Row from "./row.js"
export * as Sections from "./sections.js"
export * as SenderOwned from "./sender-owned.js"
export * as Shape from "./shape.js"
export * as StackOwned from "./stack-owned.js"

/**
 * Call facts, capture side (ADR-0061): the one place a rule reads a call of a
 * capture through. A rule asks here whether a leg was answered, how it ended
 * and who ended it, and reads a message's content through a timeline entry's
 * source pointer; it walks no flows document itself.
 *
 * Two levels: the facts of one leg at one position (`./leg-facts.ts`), and the
 * facts of a call over the legs its caller names (`./call-of-legs.ts`). Several
 * positions' messages in one capture order: `./merged-timeline.ts`.
 */
export * from "./call-of-legs.js"
export * from "./leg-facts.js"
export * from "./merged-timeline.js"

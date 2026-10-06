/**
 * The Call-ID DERIVATION predicate: whether one leg's Call-ID is the one an
 * application server minted off another's.
 *
 * It is the whole of the cut's joiner (`./cut.ts`), and it is the one place a
 * deployment's own convention shows through — `1-<base>`, a counter prefix, a
 * fixed prefix, a length floor under which a match is coincidence. So it is a
 * parameter, and the default here derives NOTHING: guessing a shape would
 * silently join two calls of a deployment whose server mints unrelated
 * Call-IDs, which reads on the wire as one call it never placed.
 *
 * One shape is fixed: a derived Call-ID ENDS WITH its base and is longer. The
 * cut finds a leg's candidate bases by looking its Call-ID's proper suffixes up
 * in an index of every Call-ID, and asks the predicate about those alone, so a
 * predicate that held elsewhere would join nothing there.
 */

/**
 * Whether `derived` is the Call-ID an application server minted off `base`.
 * May hold only where `derived` ends with `base` and is longer than it.
 */
export type CallIdDerivation = (base: string, derived: string) => boolean

/** The neutral default: no Call-ID derives from another until a deployment says how. */
export const neverDerives: CallIdDerivation = () => false

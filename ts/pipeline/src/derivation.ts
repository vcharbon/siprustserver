/**
 * The Call-ID DERIVATION predicate: whether one leg's Call-ID is the one an
 * application server minted off another's.
 *
 * It is the whole of the cut's joiner (`./cut.ts`), and it is the one place a
 * deployment's own convention shows through — `1-<base>`, a fixed prefix, a
 * suffix, an embedded token. So it is a parameter, and the default here derives
 * NOTHING: guessing a shape would silently join two calls of a deployment whose
 * server mints unrelated Call-IDs, which reads on the wire as one call it never
 * placed.
 */

/** Whether `derived` is the Call-ID an application server minted off `base`. */
export type CallIdDerivation = (base: string, derived: string) => boolean

/** The neutral default: no Call-ID derives from another until a deployment says how. */
export const neverDerives: CallIdDerivation = () => false

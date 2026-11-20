/**
 * A memo of a BOUNDED number of entries, the least recently asked for dropped
 * first: what a server holds so a reader's next question about the same thing
 * costs nothing, without the answers to a session's every question piling up.
 *
 * A key names what the entry is true of, and nothing here ever revisits it: an
 * entry stands until it is evicted, so a caller whose key can name two different
 * things states which one it holds by the key it asks under.
 */

export interface Memo<A> {
  /** The entry `key` names, if it is still held; asking for it makes it recent. */
  readonly get: (key: string) => A | undefined
  /** Holds `value` under `key`, dropping the oldest entry where that overflows. */
  readonly put: (key: string, value: A) => void
}

/** A memo of at most `bound` entries. */
export const make = <A>(bound: number): Memo<A> => {
  // Insertion order IS the use order: a hit re-inserts, so the first key is the
  // least recently used one.
  const held = new Map<string, A>()
  return {
    get: (key) => {
      const value = held.get(key)
      if (value === undefined) return undefined
      held.delete(key)
      held.set(key, value)
      return value
    },
    put: (key, value) => {
      held.delete(key)
      held.set(key, value)
      while (held.size > bound) {
        const oldest = held.keys().next()
        if (oldest.done === true) break
        held.delete(oldest.value)
      }
    }
  }
}

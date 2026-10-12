/**
 * How big a JSON document is on screen, and whether it opens folded. A
 * document past the fold is mounted only when asked for: the case document
 * and the mock's ledger of a large capture are tens of KB each, and a page
 * that stringifies and mounts every one of them is the wall.
 */

/** Characters of pretty text above which a document opens folded. */
export const JSON_FOLD_CHARS = 2000

export interface JsonSize {
  /** The document, pretty-printed with two spaces. */
  readonly text: string
  /** UTF-8 bytes of `text`: the size as sent, not the character count. */
  readonly bytes: number
  readonly lines: number
  readonly folded: boolean
}

const encoder = new TextEncoder()

export const describeJson = (value: unknown): JsonSize => {
  const text = JSON.stringify(value, null, 2) ?? "undefined"
  let lines = 1
  for (let at = text.indexOf("\n"); at !== -1; at = text.indexOf("\n", at + 1)) lines += 1
  return { text, bytes: encoder.encode(text).length, lines, folded: text.length > JSON_FOLD_CHARS }
}

/** `bytes` as a reader reads it: B below a KB, else one decimal of KB or MB. */
export const formatBytes = (bytes: number): string =>
  bytes < 1024
    ? `${bytes} B`
    : bytes < 1024 * 1024
    ? `${(bytes / 1024).toFixed(1)} KB`
    : `${(bytes / (1024 * 1024)).toFixed(1)} MB`

/** The reader's show/hide choice, and the text it was made on. */
export interface FoldChoice {
  readonly text: string
  readonly shown: boolean
}

/** The choice as it applies to `text`: made on this very document, else none — a new document starts afresh. */
export const choiceFor = (choice: FoldChoice | undefined, text: string): boolean | undefined =>
  choice !== undefined && choice.text === text ? choice.shown : undefined

/** Whether the text is mounted: the reader's choice, else open unless folded. */
export const isOpen = (folded: boolean, shown: boolean | undefined): boolean => shown ?? !folded

/** Whether the bar offers show/hide: a folded document, or one the reader already toggled. */
export const hasToggle = (folded: boolean, shown: boolean | undefined): boolean => folded || shown !== undefined

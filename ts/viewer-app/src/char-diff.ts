/**
 * Expected beside seen, character by character: the longest common
 * subsequence of the two, each side's characters outside it marked, so two
 * swapped digits in the middle of a long URI stand out. Runs of one kind are
 * merged into one part.
 */

export interface Part {
  readonly text: string
  /** Whether the other side lacks it. */
  readonly mark: boolean
}

export interface Diff {
  readonly expected: readonly Part[]
  readonly seen: readonly Part[]
}

/** Above this many cells the table is not built: both sides are marked whole. */
const MAX_CELLS = 4_000_000

const push = (parts: Array<{ text: string; mark: boolean }>, char: string, mark: boolean): void => {
  const last = parts[parts.length - 1]
  if (last !== undefined && last.mark === mark) last.text += char
  else parts.push({ text: char, mark })
}

export const charDiff = (expected: string | null, seen: string | null): Diff => {
  const a = expected ?? ""
  const b = seen ?? ""
  if ((a.length + 1) * (b.length + 1) > MAX_CELLS) {
    return { expected: a === "" ? [] : [{ text: a, mark: true }], seen: b === "" ? [] : [{ text: b, mark: true }] }
  }
  const width = b.length + 1
  const table = new Uint32Array((a.length + 1) * width)
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * width + j] = a[i] === b[j]
        ? table[(i + 1) * width + j + 1]! + 1
        : Math.max(table[(i + 1) * width + j]!, table[i * width + j + 1]!)
    }
  }
  const left: Array<{ text: string; mark: boolean }> = []
  const right: Array<{ text: string; mark: boolean }> = []
  let i = 0
  let j = 0
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      push(left, a[i]!, false)
      push(right, b[j]!, false)
      i += 1
      j += 1
    } else if (j < b.length && (i === a.length || table[i * width + j + 1]! >= table[(i + 1) * width + j]!)) {
      push(right, b[j]!, true)
      j += 1
    } else {
      push(left, a[i]!, true)
      i += 1
    }
  }
  return { expected: left, seen: right }
}

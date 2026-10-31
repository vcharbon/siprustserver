/** A count and its noun, the noun plural unless the count is 1: `1 run`, `3 runs`, `2 entries`. */
export const plural = (count: number, noun: string, many = `${noun}s`): string =>
  `${count} ${count === 1 ? noun : many}`

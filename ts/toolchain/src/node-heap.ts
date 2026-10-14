/**
 * A node process's V8 heap, sized to the memory scope it runs in.
 *
 * A scope caps the whole process tree; a heap left at V8's own default stops
 * a big job at a few gigabytes whatever the scope allows, and one larger than
 * the scope dies by the cgroup instead of by a named heap error. So the heap
 * is the scope less a share for everything that is not heap. A
 * `--max-old-space-size` the caller put in `NODE_OPTIONS` is a deliberate
 * choice and always wins; the launch says so where the scope states a size
 * too.
 */

/** The share of the scope the V8 old space may take; the rest is code, buffers and young space. */
const HEAP_SHARE = 0.8

/** systemd's size suffixes (`parse_size`, base 1024): upper case only, as systemd takes them. */
const UNITS: Record<string, number> = {
  "": 1,
  B: 1,
  K: 1024,
  M: 1024 ** 2,
  G: 1024 ** 3,
  T: 1024 ** 4,
  P: 1024 ** 5,
  E: 1024 ** 6
}

/** One component of a systemd size: a number, a fraction, a suffix. */
const COMPONENT = /\s*(\d+(?:\.\d+)?)\s*([BKMGTPE]?)/y

/**
 * The bytes of a memory size as `systemd-run -p MemoryMax=` takes it: one or
 * more `<number>[B|K|M|G|T|P|E]` components, summed (`1G512M`). `undefined`
 * for any other form — a lower-case unit, `infinity`, a percentage — none of
 * which states an absolute size here.
 */
export const bytesOf = (size: string): number | undefined => {
  if (size.trim() === "") return undefined
  let bytes = 0
  let at = 0
  while (at < size.length && size.slice(at).trim() !== "") {
    COMPONENT.lastIndex = at
    const m = COMPONENT.exec(size)
    if (m === null) return undefined
    bytes += Number(m[1]) * UNITS[m[2]!]!
    at = COMPONENT.lastIndex
  }
  return bytes
}

/** The old-space size, in MiB, for a scope of `memoryMax`; `undefined` where it states no absolute size. */
export const heapMbOf = (memoryMax: string): number | undefined => {
  const bytes = bytesOf(memoryMax)
  if (bytes === undefined) return undefined
  const mb = Math.floor((bytes * HEAP_SHARE) / 1024 ** 2)
  return mb > 0 ? mb : undefined
}

const HEAP_OPTION = "--max-old-space-size"

/**
 * `options` with the scope's heap added where the caller stated none. A heap
 * the caller stated stays, and `note` says it won where the scope had one.
 */
export const withHeap = (
  options: string | undefined,
  heapMb: number | undefined
): { readonly options: string | undefined; readonly note?: string } => {
  const all = (options ?? "").split(/\s+/).filter((o) => o !== "")
  const own = all.find((o) => o.startsWith(HEAP_OPTION))
  if (own !== undefined) {
    return heapMb === undefined
      ? { options: all.join(" ") }
      : { options: all.join(" "), note: `the caller's ${own} wins over the ${heapMb} MiB the memory scope allows` }
  }
  const sized = heapMb === undefined ? all : [...all, `${HEAP_OPTION}=${heapMb}`]
  return { options: sized.length === 0 ? undefined : sized.join(" ") }
}

/**
 * The environment a node child of a `memoryMax` scope runs under: `env` with
 * its `NODE_OPTIONS` carrying the scope's heap (or the caller's own), and what
 * to say of it.
 */
export const environment = (
  memoryMax: string,
  env: Readonly<Record<string, string | undefined>>
): { readonly env: Record<string, string>; readonly note?: string } => {
  const heap = withHeap(env["NODE_OPTIONS"], heapMbOf(memoryMax))
  const rest: Record<string, string> = {}
  for (const [key, value] of Object.entries(env)) {
    if (key !== "NODE_OPTIONS" && value !== undefined) rest[key] = value
  }
  return {
    env: heap.options === undefined ? rest : { ...rest, NODE_OPTIONS: heap.options },
    ...(heap.note === undefined ? {} : { note: heap.note })
  }
}

export * as NodeHeap from "./node-heap.js"

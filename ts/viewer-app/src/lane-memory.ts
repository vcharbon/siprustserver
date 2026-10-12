/**
 * Which lane a page opens on. A lane chosen in any picker is remembered per
 * browser under one key; a page whose URL carries no `lane` reads the memory,
 * a URL that carries one wins, and the configured lane (the empty value)
 * clears it. The pure core reads and writes a `Storage`-like store; the
 * browser binding is `window.localStorage`, which a private window makes
 * throw — read as nothing remembered, written nothing.
 */

export const LANE_KEY = "flow-viewer.lane"

/** The three `Storage` methods the memory uses. */
export interface LaneStore {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
  removeItem(key: string): void
}

/** The remembered lane, or nothing where none is stored, empty, or the store throws. */
export const recall = (store: LaneStore | undefined): string | undefined => {
  try {
    const lane = store?.getItem(LANE_KEY)
    return lane === null || lane === undefined || lane === "" ? undefined : lane
  } catch {
    return undefined
  }
}

/** Stores the lane; the empty (configured) lane clears the memory. A throwing store is left as it is. */
export const remember = (store: LaneStore | undefined, lane: string): void => {
  try {
    if (lane === "") store?.removeItem(LANE_KEY)
    else store?.setItem(LANE_KEY, lane)
  } catch {
    // A store that refuses a write remembers nothing, which the next read handles.
  }
}

/** The lane a page opens on: the URL's, else the remembered one, else the configured lane. */
export const resolve = (urlLane: string | null, stored: string | undefined): string =>
  urlLane !== null && urlLane !== "" ? urlLane : stored ?? ""

const browserStore = (): LaneStore | undefined => {
  try {
    return window.localStorage
  } catch {
    return undefined
  }
}

/** The lane this browser remembers. */
export const rememberedLane = (): string | undefined => recall(browserStore())

/** Remembers a picked lane in this browser. */
export const rememberLane = (lane: string): void => remember(browserStore(), lane)

/** The lane a page opens on, from its query string and this browser's memory. */
export const laneOf = (query: URLSearchParams): string => resolve(query.get("lane"), rememberedLane())

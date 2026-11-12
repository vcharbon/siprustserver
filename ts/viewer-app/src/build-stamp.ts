/**
 * The bundle's build stamp: the commit it was built from and when, fixed by
 * the build (`vite.config.ts` defines `__SIPRIG_BUILD__`); a bundle no build
 * stamped (a test, a bare transpile) carries none.
 */

/** What a build states of itself. */
export interface BuildStamp {
  /** The short commit of the outermost checkout built, `-dirty` where its tree differs from it; null where none reads. */
  readonly commit: string | null
  /** The build's start, an ISO-8601 UTC instant. */
  readonly builtAt: string
}

declare const __SIPRIG_BUILD__: BuildStamp | undefined

/** This bundle's stamp; undefined where no build stamped it. */
export const BUILD: BuildStamp | undefined = typeof __SIPRIG_BUILD__ === "undefined" ? undefined : __SIPRIG_BUILD__

/** The stamp as the top bar shows it: `<commit> · <YYYY-MM-DD HH:MM> UTC`. */
export const stampLine = (stamp: BuildStamp | undefined): string => {
  if (stamp === undefined) return "unstamped"
  const at = `${stamp.builtAt.slice(0, 10)} ${stamp.builtAt.slice(11, 16)} UTC`
  return `${stamp.commit ?? "no commit"} · ${at}`
}

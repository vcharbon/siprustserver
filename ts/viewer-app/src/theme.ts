/**
 * The colour theme: `auto` follows `prefers-color-scheme`, `light` and `dark`
 * pin it by `data-theme` on `<html>` (the token blocks of `styles.css` read
 * it). The choice is remembered per browser under one key; a store that
 * throws, as a private window's does, reads as `auto` and remembers nothing.
 */

export const THEME_KEY = "flow-viewer.theme"

export type Theme = "auto" | "light" | "dark"

/** The two `Storage` methods the theme uses. */
export interface ThemeStore {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

const THEMES: readonly Theme[] = ["auto", "light", "dark"]

const isTheme = (value: string | null | undefined): value is Theme =>
  value !== null && value !== undefined && (THEMES as readonly string[]).includes(value)

/** The stored theme, `auto` where none, an unknown value, or a throwing store. */
export const recallTheme = (store: ThemeStore | undefined): Theme => {
  try {
    const value = store?.getItem(THEME_KEY)
    return isTheme(value) ? value : "auto"
  } catch {
    return "auto"
  }
}

/** Stores the theme; a throwing store is left as it is. */
export const rememberTheme = (store: ThemeStore | undefined, theme: Theme): void => {
  try {
    store?.setItem(THEME_KEY, theme)
  } catch {
    // Nothing remembered: the next read is `auto`.
  }
}

/** The switch's next state: auto → light → dark → auto. */
export const nextTheme = (theme: Theme): Theme => THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length]!

/** Sets or clears `data-theme` on the root element. */
export const applyTheme = (root: { setAttribute(name: string, value: string): void; removeAttribute(name: string): void }, theme: Theme): void => {
  if (theme === "auto") root.removeAttribute("data-theme")
  else root.setAttribute("data-theme", theme)
}

const browserStore = (): ThemeStore | undefined => {
  try {
    return window.localStorage
  } catch {
    return undefined
  }
}

/** The theme this browser remembers. */
export const rememberedTheme = (): Theme => recallTheme(browserStore())

/** Remembers and applies a theme in this browser. */
export const setTheme = (theme: Theme): void => {
  rememberTheme(browserStore(), theme)
  applyTheme(document.documentElement, theme)
}

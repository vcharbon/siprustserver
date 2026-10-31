/** The top bar's theme switch: one button cycling auto → light → dark. */
import type { JSX } from "react"
import { useState } from "react"
import { nextTheme, rememberedTheme, setTheme, type Theme } from "../theme.ts"

const GLYPH: Readonly<Record<Theme, string>> = { auto: "◐ auto", light: "☀ light", dark: "☾ dark" }

export const ThemeSwitch = (): JSX.Element => {
  const [theme, setState] = useState<Theme>(rememberedTheme)
  return (
    <button
      type="button"
      className="theme-switch"
      title="theme: auto · light · dark"
      onClick={() => {
        const next = nextTheme(theme)
        setTheme(next)
        setState(next)
      }}
    >
      {GLYPH[theme]}
    </button>
  )
}

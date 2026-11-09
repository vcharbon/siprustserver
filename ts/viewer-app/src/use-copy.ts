/**
 * A copy button's state: `copy` puts text on the clipboard, `label` reads
 * `copied` or `copy refused` as the copy went for a moment, then the button's
 * own label again.
 */
import { useEffect, useRef, useState } from "react"
import { copyText } from "./clipboard.ts"

type CopyState = "idle" | "copied" | "refused"

/** How long a button states the outcome of its copy, in ms. */
const SHOWN_MS = 1600

export const useCopy = (): { readonly copy: (text: string) => void; readonly label: (idle: string) => string } => {
  const [state, setState] = useState<CopyState>("idle")
  const timer = useRef<number | undefined>(undefined)
  useEffect(() => () => window.clearTimeout(timer.current), [])
  const copy = (text: string): void => {
    void copyText(text).then((copied) => {
      setState(copied ? "copied" : "refused")
      window.clearTimeout(timer.current)
      timer.current = window.setTimeout(() => setState("idle"), SHOWN_MS)
    })
  }
  const label = (idle: string): string => (state === "copied" ? "copied" : state === "refused" ? "copy refused" : idle)
  return { copy, label }
}

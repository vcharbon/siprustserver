/**
 * Copy-as-text: the ladder on screen, in the `source.txt` line shape, on the
 * clipboard — so what a reader is looking at is what an agent gets pasted.
 *
 * The text is rendered on the click, never on every render: a study page holds
 * three ladders and a capture ladder is thousands of lines.
 */
import { useState, type JSX } from "react"

type State = "idle" | "copied" | "failed"

export const CopyText = (
  { text, label = "copy as text", title }: {
    /** Rendered when the button is pressed. */
    readonly text: () => string
    readonly label?: string
    readonly title?: string | undefined
  }
): JSX.Element => {
  const [state, setState] = useState<State>("idle")
  return (
    <button
      type="button"
      className="copy-ref"
      title={title ?? "the ladder as text, one line per message"}
      onClick={() => {
        const flip = (next: State): void => {
          setState(next)
          window.setTimeout(() => setState("idle"), 1600)
        }
        void navigator.clipboard.writeText(text()).then(() => flip("copied"), () => flip("failed"))
      }}
    >
      {state === "copied" ? "copied" : state === "failed" ? "copy refused" : label}
    </button>
  )
}

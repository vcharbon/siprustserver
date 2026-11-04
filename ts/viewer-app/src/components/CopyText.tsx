/**
 * Copy-as-text: the ladder on screen, in the `source.txt` line shape, on the
 * clipboard — so what a reader is looking at is what an agent gets pasted.
 *
 * The text is rendered on the click, never on every render: a study page holds
 * three ladders and a capture ladder is thousands of lines.
 */
import type { JSX } from "react"
import { useCopy } from "../use-copy.ts"

export const CopyText = (
  { text, label = "copy as text", title }: {
    /** Rendered when the button is pressed. */
    readonly text: () => string
    readonly label?: string
    readonly title?: string | undefined
  }
): JSX.Element => {
  const copy = useCopy()
  return (
    <button
      type="button"
      className="copy-ref"
      title={title ?? "ladder as text, one line per message"}
      onClick={() => copy.copy(text())}
    >
      {copy.label(label)}
    </button>
  )
}

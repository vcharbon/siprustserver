/**
 * A JSON blob, readable and copyable. The bar states the label, the size and
 * the line count; a document past `JSON_FOLD_CHARS` opens folded and mounts
 * its `<pre>` only once shown, a smaller one opens as it is. The text is
 * stringified once per value.
 */
import type { JSX } from "react"
import { useMemo, useState } from "react"
import { choiceFor, describeJson, type FoldChoice, formatBytes, hasToggle, isOpen } from "../json-size.ts"

export const JsonView = ({ value, label }: { value: unknown; label?: string }): JSX.Element => {
  const size = useMemo(() => describeJson(value), [value])
  const [copied, setCopied] = useState(false)
  // The choice is remembered with the text it was made on: a hidden large
  // step must not leave the next, small step closed with no toggle.
  const [choice, setChoice] = useState<FoldChoice | undefined>(undefined)
  const shown = choiceFor(choice, size.text)
  const open = isOpen(size.folded, shown)
  return (
    <div className="json-view">
      <div className="json-view-bar">
        <span>
          {label ?? "json"} · {formatBytes(size.bytes)} · {size.lines} lines
        </span>
        <span className="json-view-actions">
          {hasToggle(size.folded, shown) && (
            <button type="button" onClick={() => setChoice({ text: size.text, shown: !open })}>
              {open ? "hide" : "show"}
            </button>
          )}
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard?.writeText(size.text)
              setCopied(true)
              window.setTimeout(() => setCopied(false), 1200)
            }}
          >
            {copied ? "copied" : "copy"}
          </button>
        </span>
      </div>
      {open && <pre>{size.text}</pre>}
    </div>
  )
}

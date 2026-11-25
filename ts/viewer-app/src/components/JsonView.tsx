/**
 * A JSON blob, readable and copyable. The bar states the label, the size and
 * the line count; a document past `JSON_FOLD_CHARS` opens folded and mounts
 * its `<pre>` only once shown, a smaller one opens as it is. The text is
 * stringified once per value. Given `packed`, the same document with its
 * packed strings as they came (`json-decoded.ts`), and where it differs, the
 * bar switches between the two; copy copies the form shown, and the fold
 * choice holds across the switch.
 */
import type { JSX } from "react"
import { useMemo, useState } from "react"
import {
  choiceFor,
  describeJson,
  type FoldChoice,
  formatBytes,
  hasToggle,
  isOpen,
  isSwitchable,
  shownForm
} from "../json-size.ts"
import { useCopy } from "../use-copy.ts"

export const JsonView = ({ value, packed, label }: { value: unknown; packed?: unknown; label?: string }): JSX.Element => {
  const opened = useMemo(() => describeJson(value), [value])
  const original = useMemo(() => (packed === undefined ? undefined : describeJson(packed)), [packed])
  const copy = useCopy()
  // The choice is remembered with the text it was made on: a hidden large
  // step must not leave the next, small step closed with no toggle.
  const [choice, setChoice] = useState<FoldChoice | undefined>(undefined)
  const [asPacked, setAsPacked] = useState(false)
  const size = shownForm(opened, original, asPacked)
  const shown = choiceFor(choice, opened.text)
  const open = isOpen(size.folded, shown)
  return (
    <div className="json-view">
      <div className="json-view-bar">
        <span>
          {label ?? "json"} · {formatBytes(size.bytes)} · {size.lines} lines
        </span>
        <span className="json-view-actions">
          {isSwitchable(opened, original) && (
            <button
              type="button"
              title={asPacked ? "packed strings opened" : "packed strings as base64 gzip"}
              onClick={() => setAsPacked(!asPacked)}
            >
              {asPacked ? "unpack" : "gzip"}
            </button>
          )}
          {hasToggle(size.folded, shown) && (
            <button type="button" onClick={() => setChoice({ text: opened.text, shown: !open })}>
              {open ? "hide" : "show"}
            </button>
          )}
          <button
            type="button"
            onClick={() => copy.copy(size.text)}
          >
            {copy.label("copy")}
          </button>
        </span>
      </div>
      {open && <pre>{size.text}</pre>}
    </div>
  )
}

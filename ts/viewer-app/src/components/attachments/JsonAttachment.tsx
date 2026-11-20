/** A `json` attachment: the document as a JSON view, its packed strings opened (`json-decoded.ts`). */
import type { JSX } from "react"
import { useAsync } from "../../api.ts"
import { jsonDecoded } from "../../json-decoded.ts"
import { JsonView } from "../JsonView.tsx"

export const JsonAttachment = ({ id, value, label }: {
  readonly id: number
  readonly value: unknown
  readonly label: string
}): JSX.Element => {
  const opened = useAsync(() => jsonDecoded(value), `json#${id}`)
  if (opened.state === "loading") return <p className="muted">loading…</p>
  return <JsonView value={opened.state === "ok" ? opened.value : value} label={label} />
}

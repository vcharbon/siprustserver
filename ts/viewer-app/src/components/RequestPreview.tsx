/**
 * A preview as the page shows it (§5 R1): per lane the count and the
 * captures, each `full` or `cases`, folded behind the count past a handful.
 */
import type { JSX } from "react"
import type { RequestLanePreview, RequestPreview as Preview } from "../api.ts"
import { FOLD_CAPTURES, listedCaptures, previewLine } from "../request-builder.ts"

const CaptureList = ({ one }: { readonly one: RequestLanePreview }): JSX.Element => {
  const listed = listedCaptures(one)
  const list = (
    <ul className="request-captures mono">
      {listed.map((capture) => (
        <li key={capture.capture}>
          {capture.capture} <span className="tag">{capture.kind}</span> {capture.cells} cell(s)
        </li>
      ))}
    </ul>
  )
  if (listed.length <= FOLD_CAPTURES) return list
  return (
    <details className="request-fold">
      <summary>{listed.length} captures</summary>
      {list}
    </details>
  )
}

export const RequestPreviewPane = ({ preview }: { readonly preview: Preview }): JSX.Element => (
  <div className="request-preview">
    <p className="muted">against prep {preview.prepId}</p>
    {preview.lanes.map((one) => (
      <div key={one.lane} className="request-lane">
        <b>{one.lane}</b>: {previewLine(one)}
        <CaptureList one={one} />
      </div>
    ))}
  </div>
)

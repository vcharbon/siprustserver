/**
 * The verification lines of a cell, a batch or a job as a table: each line's
 * answer, name, source, message, and the step, expected, seen and side its
 * detail states, a waived or informative line marked; the cell's status in
 * the head where there is one.
 */
import type { JSX } from "react"
import type { CellStatus, VerificationLine } from "../api.ts"
import { ANSWER_MARK, answerTone, detailOf, statusTone } from "../verification-lines.ts"
import { SideTag } from "./CheckCard.tsx"

/** A cell's status as a chip toned like its dot. */
export const StatusChip = ({ status }: { readonly status: CellStatus }): JSX.Element => (
  <span className={`cp-dot vl-status ${statusTone(status)}`} title="status">{status}</span>
)

export const VerificationLines = ({ lines, status, sides = [], title = "Lines", subtitle }: {
  readonly lines: readonly VerificationLine[]
  readonly status?: CellStatus | undefined
  /** The side vocabulary that tones a line's side. */
  readonly sides?: readonly string[]
  readonly title?: string
  readonly subtitle?: string | undefined
}): JSX.Element => (
  <section className="cp-panel vl">
    <h3>
      {title} <span className="muted">{lines.length}</span>
      {status === undefined ? null : <> <StatusChip status={status} /></>}
      {subtitle === undefined ? null : <span className="muted mono cp-small">{subtitle}</span>}
    </h3>
    {lines.length === 0 ? <p className="muted">none</p> : (
      <table className="cp-table vl-table">
        <thead>
          <tr>
            <th />
            <th>line</th>
            <th>source</th>
            <th>message</th>
            <th>step</th>
            <th>expected</th>
            <th>seen</th>
            <th>side</th>
          </tr>
        </thead>
        <tbody>
          {lines.map((line, at) => {
            const detail = detailOf(line)
            return (
              <tr key={at} className={line.waived || line.informative ? "vl-aside" : undefined}>
                <td>
                  <span className={`cp-dot ${answerTone(line)}`} title={line.answer}>{ANSWER_MARK[line.answer]}</span>
                </td>
                <td className="mono">
                  {line.name}
                  {line.waived && <> <span className="cp-tag vl-waived" title="known bug of the lane">waived</span></>}
                  {line.informative && <> <span className="cp-tag" title="decides nothing">informative</span></>}
                </td>
                <td className="muted">{line.source}</td>
                <td className="mono vl-message">{line.message ?? ""}</td>
                <td className="mono">{detail.step ?? ""}</td>
                <td className="mono cp-expected">{detail.expected ?? ""}</td>
                <td className="mono">{detail.seen ?? ""}</td>
                <td>{detail.side === null ? null : <SideTag side={detail.side} sides={sides} />}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    )}
  </section>
)

/** One failed check as a card: where and what, expected beside seen character by character, what the decision said, whose side. */
import type { JSX, ReactNode } from "react"
import type { CampaignCheck } from "../api.ts"
import { charDiff, type Part } from "../char-diff.ts"

const Parts = ({ parts, kind }: { readonly parts: readonly Part[]; readonly kind: "del" | "ins" }): JSX.Element => (
  <span className="cp-diff">
    {parts.map((part, at) =>
      part.mark ? (kind === "del" ? <del key={at}>{part.text}</del> : <ins key={at}>{part.text}</ins>) : (
        <span key={at}>{part.text}</span>
      )
    )}
  </span>
)

/** The tag of a side, toned by its place in the deployment's side vocabulary. */
export const SideTag = (
  { side, sides, count }: { readonly side: string; readonly sides: readonly string[]; readonly count?: number }
): JSX.Element => {
  const at = sides.indexOf(side)
  const tone = side === "error" ? "error" : at < 0 ? "untyped" : `side${at}`
  return <span className={`cp-tag ${tone}`}>{side}{count === undefined ? "" : ` ${count}`}</span>
}

export const CheckCard = ({ check, sides, saidLabel, head, said }: {
  readonly check: CampaignCheck
  readonly sides: readonly string[]
  readonly saidLabel: string
  /** What leads the card, in place of the check's own anchor and field. */
  readonly head?: ReactNode
  /** What follows the said line: the log link. */
  readonly said?: ReactNode
}): JSX.Element => {
  const diff = charDiff(check.expected, check.seen)
  return (
    <div className="cp-card">
      <div className="cp-card-head">
        {head ?? <span className="mono">✗ {check.anchor} · {check.field}</span>}
        <SideTag side={check.side ?? "untyped"} sides={sides} />
      </div>
      <dl className="cp-kv">
        <dt>expected</dt>
        <dd>{check.expected === null ? <span className="muted">—</span> : <Parts parts={diff.expected} kind="del" />}</dd>
        <dt>seen</dt>
        <dd>{check.seen === null ? <span className="cp-nok">(absent)</span> : <Parts parts={diff.seen} kind="ins" />}</dd>
        <dt>{saidLabel}</dt>
        <dd className="mono">{check.said ?? <span className="muted">—</span>}{said}</dd>
      </dl>
    </div>
  )
}

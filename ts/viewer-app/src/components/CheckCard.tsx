/**
 * One failed check as a card: where and what, expected beside seen, what the
 * decision said, whose side. Two values of one thing read character by
 * character; a statement the seen broke reads plain, with no empty said line.
 * Where the host can show the message the check was made at, the anchor is a
 * link to it.
 */
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

/** Whether a check's expected and seen are compared character by character: only two values of one thing are. */
export const comparesCharacters = (check: CampaignCheck): boolean =>
  check.compare !== "statement" && check.expected !== null && check.seen !== null

export const CheckCard = ({ check, sides, saidLabel, head, said, onAnchor }: {
  readonly check: CampaignCheck
  readonly sides: readonly string[]
  readonly saidLabel: string
  /** What leads the card, in place of the check's own anchor and field. */
  readonly head?: ReactNode
  /** What follows the said line: the log link. */
  readonly said?: ReactNode
  /** Shows the message the check was made at; absent where there is none to show. */
  readonly onAnchor?: (() => void) | undefined
}): JSX.Element => {
  const diff = charDiff(check.expected, check.seen)
  const byCharacter = comparesCharacters(check)
  const saidShown = check.compare !== "statement" || check.said !== null || said !== undefined
  return (
    <div className="cp-card">
      <div className="cp-card-head">
        {head ?? (
          <span className="mono">
            ✗{" "}
            {onAnchor === undefined ? check.anchor : (
              <button type="button" className="cp-anchor" title="show the message in the ladder" onClick={onAnchor}>
                {check.anchor} →
              </button>
            )}{" "}
            · {check.field}
          </span>
        )}
        <SideTag side={check.side ?? "untyped"} sides={sides} />
      </div>
      <dl className="cp-kv">
        <dt>expected</dt>
        <dd>
          {check.expected === null
            ? <span className="muted">—</span>
            : byCharacter
            ? <Parts parts={diff.expected} kind="del" />
            : <span className="cp-diff">{check.expected}</span>}
        </dd>
        <dt>seen</dt>
        <dd>
          {check.seen === null
            ? <span className="cp-nok">(absent)</span>
            : byCharacter
            ? <Parts parts={diff.seen} kind="ins" />
            : <span className="cp-diff cp-nok">{check.seen}</span>}
        </dd>
        {saidShown && (
          <>
            <dt>{saidLabel}</dt>
            <dd className="mono">{check.said ?? <span className="muted">—</span>}{said}</dd>
          </>
        )}
      </dl>
    </div>
  )
}

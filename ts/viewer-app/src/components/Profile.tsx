/**
 * What the lane's HTTP mock ANSWERS for this case, verbatim.
 *
 * The panel draws the `MockAnswers` the server read off the deployment's
 * record of the case and derives nothing of its own: the target the mock
 * answers for, what the capture observed, then one row per answer — its
 * label, the request it matches, the answer as a `JsonView` (folded past its
 * size). A list of more than `OPEN_ENTRIES` answers folds whole under its count.
 */
import type { JSX } from "react"
import type { MockAnswer, MockAnswers } from "../api.ts"
import { JsonView } from "./JsonView.tsx"

/** Answers a list shows open; past this the list folds, its summary stating the count. */
const OPEN_ENTRIES = 3

export const Profile = ({ ledger }: { ledger?: MockAnswers | null | undefined }): JSX.Element => {
  if (ledger === undefined || ledger === null) {
    return (
      <section className="case-profile">
        <h3>mock answers</h3>
        <p className="profile-source">no mock record beside this document — the mock answers from its own scenario</p>
      </section>
    )
  }
  const answers = ledger.answers
  return (
    <section className="case-profile">
      <h3>mock answers</h3>
      <p className="profile-source">
        for <code>{ledger.target ?? "?"}</code>
        {ledger.observed !== null && <> · captured <code>{ledger.observed}</code></>}
      </p>
      {answers.length === 0
        ? <p className="profile-source">the record states no answer — the mock answers nothing for this case</p>
        : answers.length > OPEN_ENTRIES
        ? (
          <details className="ledger-fold">
            <summary className="profile-source">{answers.length} entries</summary>
            <Answers answers={answers} />
          </details>
        )
        : <Answers answers={answers} />}
    </section>
  )
}

const Answers = ({ answers }: { answers: ReadonlyArray<MockAnswer> }): JSX.Element => (
  <ul className="profile-list">
    {answers.map((answer, i) => (
      <li key={i} className="ledger-entry">
        <div className="ledger-head">
          {answer.match !== "" && <code>{answer.match}</code>}
        </div>
        <JsonView value={answer.answer} label={answer.label} />
      </li>
    ))}
  </ul>
)

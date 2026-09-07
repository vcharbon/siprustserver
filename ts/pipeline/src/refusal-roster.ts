/**
 * What makes a roster of refusal rules well-formed, refused at LOAD time rather
 * than discovered on a corpus.
 *
 * Three ways a roster lies about itself, and each has cost a session already:
 * two rules sharing one token, so `excluded.json` carries two records nothing
 * tells apart; a token whose prefix names one party while its rule names
 * another; and a rule that names a DEPLOYMENT living in the deployment-free
 * package, which is how `source-answer-not-captured` came to justify itself with
 * a claim about `relay18x`'s arms.
 *
 * A retired token KEEPS ITS SLOT — {@link RETIRED_REFUSAL_IDS} — so a name a
 * corpus still carries on disk can never be reused for something else.
 */
import type { RefusalSubject, RuleIdentity } from "./refusal-rule.js"

/**
 * Tokens no rule mints any more, held so the names stay spent. A corpus written
 * before a rule was retired still carries its token, and a reader who meets it
 * must find one meaning for it and not two.
 */
export const RETIRED_REFUSAL_IDS: ReadonlySet<string> = new Set<string>()

/** The prefix a subject's tokens carry. */
const SUBJECT_PREFIX: Record<RefusalSubject, string> = {
  source: "source-",
  egress: "egress-",
  sut: "sut-",
  scope: "scope-"
}

/**
 * The tokens that predate the taxonomy and are carried on disk by every corpus
 * ever swept, so their subject is declared and their name is left alone. A
 * rename is a corpus migration, not a roster edit.
 */
const UNPREFIXED_REFUSAL_IDS: ReadonlySet<string> = new Set([
  "missing-upstream-leg",
  "refer-replaces-out-of-scope"
])

/** Every rule id a roster states, for a loader that must know the whole set. */
export const refusalRuleIds = (
  rules: ReadonlyArray<RuleIdentity>
): ReadonlySet<string> => new Set(rules.map((r) => r.id))

/**
 * Refuse a roster that cannot be trusted to say what it refuses: a token used
 * twice, a token already spent by a retired rule, or a subject its own token
 * contradicts.
 *
 * `deploymentFree` is the one check a package makes about ITSELF: a roster
 * inside `@sip/pipeline` states no `sut` subject, because naming what one
 * platform does is naming a deployment.
 */
export const checkRefusalRoster = (
  rules: ReadonlyArray<RuleIdentity>,
  options: { readonly where: string; readonly deploymentFree?: boolean }
): void => {
  const seen = new Set<string>()
  for (const rule of rules) {
    if (seen.has(rule.id)) {
      throw new Error(`${options.where}: two refusal rules share the token '${rule.id}'`)
    }
    if (RETIRED_REFUSAL_IDS.has(rule.id)) {
      throw new Error(`${options.where}: '${rule.id}' is a retired token and cannot be reused`)
    }
    seen.add(rule.id)
    if (options.deploymentFree === true && rule.subject === "sut") {
      throw new Error(
        `${options.where}: '${rule.id}' names the source platform's own behaviour, which a ` +
          `deployment-free roster cannot state`
      )
    }
    if (!UNPREFIXED_REFUSAL_IDS.has(rule.id) && !rule.id.startsWith(SUBJECT_PREFIX[rule.subject])) {
      throw new Error(
        `${options.where}: '${rule.id}' declares subject '${rule.subject}' and carries no ` +
          `'${SUBJECT_PREFIX[rule.subject]}' prefix`
      )
    }
  }
}

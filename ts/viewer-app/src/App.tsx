/** The shell: route in, page out. */
import type { JSX } from "react"
import { Link } from "./components/Link.tsx"
import { CasePage } from "./pages/CasePage.tsx"
import { ComparePage } from "./pages/ComparePage.tsx"
import { DeltasPage } from "./pages/DeltasPage.tsx"
import { IndexPage } from "./pages/IndexPage.tsx"
import { JobPage } from "./pages/JobPage.tsx"
import { JobsPage } from "./pages/JobsPage.tsx"
import { ReportPage } from "./pages/ReportPage.tsx"
import { RulesPage } from "./pages/RulesPage.tsx"
import { RunComparePage } from "./pages/RunComparePage.tsx"
import { RunPage } from "./pages/RunPage.tsx"
import { SourcesPage } from "./pages/SourcesPage.tsx"
import { StudyPage } from "./pages/StudyPage.tsx"
import { TriagePage } from "./pages/TriagePage.tsx"
import { href, useRoute } from "./router.ts"

export const App = (): JSX.Element => {
  const route = useRoute()
  return (
    <div className="app">
      <nav className="app-nav">
        {/* Lane-less on purpose: a page opened without a lane reads the one the
            browser remembers (`lane-memory`), and a rendered href could only
            carry the memory as it stood at render time. */}
        <Link to={href.index()} className="brand">flow viewer</Link>
        <Link to={href.triage()}>triage</Link>
        <Link to={href.rules()}>rules</Link>
        <Link to={href.sources()}>sources</Link>
        <Link to={href.jobs()}>jobs</Link>
        <Link to={href.compareRuns({})}>compare</Link>
        <Link to={href.report({})}>report</Link>
        <span className="muted">{window.location.pathname}</span>
      </nav>
      {route.kind === "index" && <IndexPage />}
      {route.kind === "case" && <CasePage id={route.id} />}
      {route.kind === "run" && <RunPage id={route.id} />}
      {route.kind === "compare" && <ComparePage left={route.left} right={route.right} />}
      {route.kind === "compare-runs" && <RunComparePage {...(route.path === undefined ? {} : { path: route.path })} />}
      {route.kind === "report" && <ReportPage {...(route.run === undefined ? {} : { run: route.run })} />}
      {route.kind === "triage" && <TriagePage />}
      {route.kind === "rules" && <RulesPage />}
      {route.kind === "sources" && <SourcesPage />}
      {route.kind === "jobs" && <JobsPage />}
      {route.kind === "job" && <JobPage id={route.id} />}
      {route.kind === "triage-study" && <StudyPage capture={route.capture} />}
      {route.kind === "deltas" && <DeltasPage />}
      {route.kind === "unknown" && (
        <p className="page error">
          no route for {route.path}. Try <Link to={href.index()}>the index</Link>.
        </p>
      )}
    </div>
  )
}

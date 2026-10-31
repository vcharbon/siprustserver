/**
 * The browser side of the router: the current route and query, and moving
 * between pages. The URL grammar itself is `@sip/viewer-core/routes`, which
 * the server reads page URLs through too; every route, its query and its data
 * are stated in that package's route table.
 */
import { type PageParams, parseRoute, queryString, type Route } from "@sip/viewer-core/routes"
import { useEffect, useState } from "react"

export {
  type CampaignParams,
  type CompareParams,
  formatSideRef,
  href,
  isSelector,
  type PageParams,
  parseRoute,
  parseSideRef,
  type QueueParams,
  type ReportParams,
  type Route,
  type RulesParams,
  type SideRef,
  type SourcesParams,
  type StudyParams
} from "@sip/viewer-core/routes"

/** The current query string, re-read on every navigation. */
export const useQuery = (): URLSearchParams => {
  const [search, setSearch] = useState(() => window.location.search)
  useEffect(() => {
    const onChange = () => setSearch(window.location.search)
    window.addEventListener("popstate", onChange)
    return () => window.removeEventListener("popstate", onChange)
  }, [])
  return new URLSearchParams(search)
}

/**
 * Rewrites the query string in place. `replaceState`, because moving the
 * selection inside one study is not a navigation — the back button belongs to
 * the pages the reader came through, not to every message they clicked.
 */
export const setQuery = (params: PageParams): void => {
  window.history.replaceState({}, "", `${window.location.pathname}${queryString(params)}`)
}

/** The page moved to `path` in place: an old URL form read as its canonical one, the back button untouched. */
export const redirect = (path: string): void => {
  window.history.replaceState({}, "", path)
  window.dispatchEvent(new PopStateEvent("popstate"))
}

export const navigate = (path: string): void => {
  window.history.pushState({}, "", path)
  window.dispatchEvent(new PopStateEvent("popstate"))
}

export const useRoute = (): Route => {
  const [route, setRoute] = useState(() => parseRoute(window.location.pathname))
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(window.location.pathname))
    window.addEventListener("popstate", onChange)
    return () => window.removeEventListener("popstate", onChange)
  }, [])
  return route
}

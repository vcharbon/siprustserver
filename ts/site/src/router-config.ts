/** The HTTP router settings every site server is served with. */
import type { FindMyWay } from "effect/unstable/http"

/**
 * A path parameter matches up to 2048 characters: a cell or case name, URL
 * encoded, runs well past the router's default of 100, and a longer one would
 * fail as a 404 before any handler sees it.
 */
export const routerConfig: Partial<FindMyWay.RouterConfig> = { maxParamLength: 2048 }

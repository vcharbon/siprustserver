/**
 * The server's `/api/config`, read once per page load and shared by the views
 * that need it: the commands they offer for copying, the site's public URL
 * the links they copy are built under.
 */
import { useEffect, useState } from "react"
import { api, type ServerConfig } from "./api.ts"

let loaded: Promise<ServerConfig | undefined> | undefined

/** The server's config; undefined where it does not answer. */
export const loadServerConfig = (): Promise<ServerConfig | undefined> =>
  loaded ??= api.config().catch(() => undefined)

/** The origin a copied link is built under: the site's public URL where the server states one, else `fallback`. */
export const linkOrigin = (config: ServerConfig | undefined, fallback: string): string =>
  config?.publicUrl === undefined || config.publicUrl === "" ? fallback : config.publicUrl.replace(/\/+$/, "")

/** The site's public URL once the config is read, else `fallback` (the page's own origin). */
export const useLinkOrigin = (fallback: string): string => {
  const [origin, setOrigin] = useState(fallback)
  useEffect(() => {
    let live = true
    void loadServerConfig().then((config) => {
      if (live) setOrigin(linkOrigin(config, fallback))
    })
    return () => {
      live = false
    }
  }, [fallback])
  return origin
}

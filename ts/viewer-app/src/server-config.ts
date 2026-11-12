/**
 * The server's `/api/config`, read once per page load and shared by the views
 * that need it: the commands they offer for copying, the site's public URL
 * the links they copy are built under, the site's name the top bar shows.
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

/** The site's name; undefined where the server states none or does not answer. */
export const siteNameOf = (config: ServerConfig | undefined): string | undefined =>
  config?.name === undefined || config.name.trim() === "" ? undefined : config.name

/** The site's name once the config is read; undefined until then and where the server states none. */
export const useSiteName = (): string | undefined => {
  const [name, setName] = useState<string | undefined>(undefined)
  useEffect(() => {
    let live = true
    void loadServerConfig().then((config) => {
      if (live) setName(siteNameOf(config))
    })
    return () => {
      live = false
    }
  }, [])
  return name
}

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

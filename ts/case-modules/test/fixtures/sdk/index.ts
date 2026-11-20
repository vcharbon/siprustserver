/** The SDK module a workspace file's specifier resolves to at run time. */
import type { Api } from "./surface.js"

export const defineGreeting: Api["defineGreeting"] = (greeting) => ({
  ...greeting,
  text: greeting.tone === "loud" ? `HELLO ${greeting.name.toUpperCase()}` : `hello ${greeting.name}`
})

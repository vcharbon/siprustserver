/**
 * The app's HTML shell made to point at the page's overview: a
 * `<link rel="alternate" type="text/markdown">` in the head and a
 * `<noscript>` block at the top of the body naming the overview and
 * `/llms.txt`, so a fetch that runs no JavaScript is told where the page's
 * data is. The browser app itself is untouched.
 */
import { absolute, linksHref, LLMS_PATH } from "./overview.ts"

const escapeHtml = (text: string): string =>
  text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;")

/** `html` (the app's `index.html`) pointing at the overview of `pageUrl`, served at `origin`. */
export const shellWithPointer = (html: string, pageUrl: string, origin: string): string => {
  const overview = escapeHtml(linksHref(origin, pageUrl))
  const llms = escapeHtml(absolute(origin, LLMS_PATH))
  const link = `<link rel="alternate" type="text/markdown" title="page overview" href="${overview}">`
  const noscript = `<noscript><p>Page overview (Markdown, absolute links): <a href="${overview}">${overview}</a>` +
    ` · site map: <a href="${llms}">${llms}</a></p></noscript>`
  const headed = html.includes("</head>") ? html.replace("</head>", `${link}\n</head>`) : `${link}\n${html}`
  const body = /<body[^>]*>/.exec(headed)
  return body === null
    ? `${headed}\n${noscript}`
    : headed.replace(body[0], `${body[0]}\n${noscript}`)
}

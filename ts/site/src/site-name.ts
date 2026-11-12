/**
 * The site's name: the rig's, then the title of the pack the server runs. The
 * one place it is formed; `/api/config` states it for the SPA's top bar and
 * tab, `/llms.txt` opens with it.
 */

/** The rig's name, which the site's name opens with. */
export const RIG = "siprig"

/** The site's name for a pack titled `title`: the rig's alone where the title is blank. */
export const siteName = (title: string): string => (title.trim() === "" ? RIG : `${RIG} · ${title}`)

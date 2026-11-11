/**
 * Every attachment kind (`attachment.ts`), in the order the site lists a
 * holder's attachments. Schema-free, so the site's bundle reads it without
 * the schema library.
 */
export const KINDS = ["records", "diff", "table", "json", "log", "file", "link"] as const

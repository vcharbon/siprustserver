# Viewer UI style

The rule for every user-visible string and colour in `viewer-app` and the
components it reuses from `viewer-core`.

## Text

1. **Title**: an explicit noun, 5 words at most.
2. **Subtitle**: optional, 8 words at most, a fragment, never a sentence.
3. **Values over explanations**: a label beside its value. No sentence says
   what a field, a section or a view is.
4. **Legends are symbols**: `✓ pass · ✗ fail · ! error · — not run`.
5. **Help**: a one-line `title` tooltip at most, terse technical English.
6. **Findings are one line**: rule · chip · party · step · the short
   difference. The full text, the RFC reference and the addresses open on
   demand (tooltip or expander). A section head is a noun and a count.

Empty states, loading lines and errors follow the same rule: `none`,
`loading…`, `<what>: <error>`.

## Colour

1. **Dark mode is first-class**: every page, every component, every SVG
   reads in both schemes. The theme switch in the top bar sets
   `data-theme` on `<html>`; with none set, `prefers-color-scheme` decides.
2. **One token set** for the whole site, defined once at the top of
   `src/styles.css` (light, then dark under
   `@media (prefers-color-scheme: dark) :root:not([data-theme="light"])` and
   again under `:root[data-theme="dark"]`). No hex, `rgb()` or named colour
   outside that block.
3. **A reused component takes its host's tokens**, never its own background:
   `viewer-core`'s `ladder.css` maps its `--fv-*` names onto the site tokens.

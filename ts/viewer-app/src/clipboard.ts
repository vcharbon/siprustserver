/**
 * Text onto the clipboard, on a secure origin and off one: the async
 * Clipboard API where the browser offers it and it accepts the write, else
 * the selected-textarea `copy` command. The result says whether either copied.
 */

/** The two ways a page can copy; each is absent or failing where the browser refuses it. */
export interface ClipboardPaths {
  /** `navigator.clipboard.writeText`; undefined where the context is not secure. */
  readonly writeText: ((text: string) => Promise<void>) | undefined
  /** The `copy` command on a selected textarea holding `text`; true where the browser copied. */
  readonly copyCommand: (text: string) => boolean
}

/** Copies `text` by the first path that takes it; false where neither did. */
export const copyWith = async (paths: ClipboardPaths, text: string): Promise<boolean> => {
  if (paths.writeText !== undefined) {
    try {
      await paths.writeText(text)
      return true
    } catch {
      // A refused write (no focus, no permission) still has the command to try.
    }
  }
  try {
    return paths.copyCommand(text)
  } catch {
    return false
  }
}

/** The `copy` command on an off-screen textarea; focus and selection are given back after. */
const copyCommand = (text: string): boolean => {
  const focused = document.activeElement instanceof HTMLElement ? document.activeElement : null
  const area = document.createElement("textarea")
  area.value = text
  area.setAttribute("readonly", "")
  area.style.position = "fixed"
  area.style.top = "0"
  area.style.left = "0"
  area.style.opacity = "0"
  area.style.pointerEvents = "none"
  document.body.appendChild(area)
  try {
    area.focus()
    area.select()
    area.setSelectionRange(0, text.length)
    return document.execCommand("copy")
  } finally {
    area.remove()
    focused?.focus({ preventScroll: true })
  }
}

/** Copies `text` in this page; true where the clipboard now holds it. */
export const copyText = (text: string): Promise<boolean> =>
  copyWith(
    {
      writeText: typeof navigator.clipboard?.writeText === "function"
        ? (value) => navigator.clipboard.writeText(value)
        : undefined,
      copyCommand
    },
    text
  )

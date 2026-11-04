/** A source file's text as the lines a page numbers: split on LF or CRLF, a final line break ending the last line, not opening one more. */
export const sourceLines = (text: string): readonly string[] => {
  const lines = text.split(/\r?\n/)
  return lines.length > 1 && lines[lines.length - 1] === "" ? lines.slice(0, -1) : lines
}

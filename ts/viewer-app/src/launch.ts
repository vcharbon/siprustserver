/**
 * The triage launch line a view offers for copying: the deployment's command
 * (`Commands.triage`), the capture, and the call and lane where they are known.
 *
 * The lane is part of the ref because the lanes are separate SUTs carrying
 * separate defects: a ref pasted without one triages the skill's default lane,
 * which is not necessarily the lane the reader was reading. The case is part
 * of it for the same reason: a ref copied off ONE call of a big capture must
 * hand that call over, not the capture it sits in.
 */
export const launchRef = (
  command: string,
  capture: string,
  lane?: string | undefined,
  caseId?: string | undefined
): string =>
  `${command} ${capture}` +
  `${caseId === undefined || caseId === "" ? "" : ` --case ${caseId}`}` +
  `${lane === undefined || lane === "" ? "" : ` --lane ${lane}`}`

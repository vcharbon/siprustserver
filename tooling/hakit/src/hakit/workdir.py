"""A docker generator's working directory (ADR-0049 §6.4).

SIPp writes its records, stat file and error file synchronously from its one
thread, so a write that waits on the host's disk stalls every call it drives.
A docker generator therefore works in a directory on tmpfs, under the lab's
generator scratch (`HAKIT_GENERATOR_SCRATCH`), mirroring the run directory's
layout; once its containers are removed the directory is copied into the
run's stream directory and dropped. Without a scratch the generator works in
the stream directory itself. Every effect goes through the executor.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .executor import Executor


@dataclass(frozen=True)
class WorkDir:
    # What the generator's containers mount at /stats.
    work: Path
    # The run's stream directory, where the files end.
    keep: Path

    @property
    def scratch(self) -> bool:
        return self.work != self.keep

    def prepare(self, executor: Executor, inputs: list[Path]) -> None:
        """Make the working directory and copy the generator's inputs into it."""
        if not self.scratch:
            return
        executor.run(["mkdir", "-p", str(self.work)], timeout_s=30)
        for p in inputs:
            executor.run(["cp", str(p), str(self.work)], timeout_s=30)

    def collect(self, executor: Executor) -> None:
        """Copy the working directory into the stream directory and drop it;
        called once the generator's containers are gone."""
        if self.scratch:
            collect_tree(executor, self.work, self.keep)


def collect_tree(executor: Executor, src: Path, dst: Path) -> bool:
    """Copy `src` into `dst`, then drop `src`; false (and `src` kept) when the
    copy failed or there was nothing to copy."""
    if executor.run(["cp", "-a", f"{src}/.", str(dst)], timeout_s=600).rc != 0:
        return False
    executor.run(["rm", "-r", "--", str(src)], timeout_s=120)
    return True


def work_dir(scratch: str, run_dir: Path, stream_dir: Path) -> WorkDir:
    """The working directory of the generator whose stream directory is
    `stream_dir`, under `run_dir`."""
    if not scratch:
        return WorkDir(stream_dir, stream_dir)
    return WorkDir(Path(scratch) / run_dir.name / stream_dir.relative_to(run_dir), stream_dir)

"""pytest configuration: every test speaks to a silent daemon from this
checkout, never a stale PATH install."""

import os
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def pytest_configure():
    # A silent backend: no audio device is ever opened by the daemons these
    # tests spawn.
    os.environ.setdefault("BO_BACKEND", "silent")
    # Point the client at the freshly built bo binary when one exists, so
    # the spawned daemon is this build (the lib also walks up from the cwd,
    # but an explicit BO_DAEMON makes the test hermetic wherever it runs).
    for profile in ("debug", "release"):
        candidate = REPO / "target" / profile / "bo"
        if candidate.is_file():
            os.environ.setdefault("BO_DAEMON", str(candidate))
            return

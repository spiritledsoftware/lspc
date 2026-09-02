"""Shared isolated user environment for release smoke tests."""

from __future__ import annotations

import os
from pathlib import Path


def isolated_user_environment(root: Path) -> dict[str, str]:
    values = os.environ.copy()
    home = root / "home"
    roaming = home / "AppData/Roaming"
    local = home / "AppData/Local"
    for directory in (home, root / "config", root / "state", roaming, local):
        directory.mkdir(parents=True, exist_ok=True)
    values.update({
        "HOME": str(home),
        "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "state"),
        "APPDATA": str(roaming),
        "LOCALAPPDATA": str(local),
    })
    return values

#!/usr/bin/env bash
# Uninstall bo from this machine: the system-wide `bo` binary (cargo
# uninstall) and the `pybo` Python package, reversing scripts/install.sh.
#
# pybo is removed from the same Python install.sh would have used:
# $BO_PYTHON if set, else the active virtualenv (VIRTUAL_ENV), else
# `python3` on PATH.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "== [1/2] uninstalling the bo binary =="
if command -v cargo >/dev/null 2>&1 && cargo uninstall bo >/dev/null 2>&1; then
  echo "== bo uninstalled (cargo uninstall bo)"
elif [ -x "$HOME/.cargo/bin/bo" ]; then
  rm -f "$HOME/.cargo/bin/bo"
  echo "== removed $HOME/.cargo/bin/bo"
else
  echo "== nothing to uninstall: no bo binary found"
fi

echo "== [2/2] uninstalling pybo =="
if [ -n "${BO_PYTHON:-}" ]; then
  PY="$BO_PYTHON"
elif [ -n "${VIRTUAL_ENV:-}" ]; then
  PY="$VIRTUAL_ENV/bin/python"
else
  PY="$(command -v python3)"
fi
if "$PY" -c "import pybo" >/dev/null 2>&1; then
  PIP_BREAK=()
  if [ "${BO_PIP_BREAK:-0}" = 1 ]; then
    PIP_BREAK=(--break-system-packages)
  fi
  if "$PY" -m pip uninstall -y "${PIP_BREAK[@]+\"${PIP_BREAK[@]}\"}" pybo; then
    echo "== pybo uninstalled from $PY"
  else
    echo "== pybo uninstall failed; on a PEP 668 host retry with BO_PIP_BREAK=1" >&2
  fi
else
  echo "== nothing to uninstall: pybo is not importable from $PY"
fi

echo "== done. Reinstall with: ./scripts/install.sh =="

#!/usr/bin/env bash
# Install bo for this machine: the `bo` CLI/daemon binary system-wide, and
# optionally the `pybo` Python package into a Python environment.
#
# Usage:
#   ./scripts/install.sh              # cargo install the bo binary
#   ./scripts/install.sh --pybo       # also install pybo (see below)
#
# pybo is installed from a locally built wheel into the Python of your
# choice: $BO_PYTHON if set, else the active virtualenv (VIRTUAL_ENV), else
# `python3` on PATH. Set BO_PIP_BREAK=1 to add --break-system-packages when
# the target interpreter refuses system installs (PEP 668).
#
# The bo binary must be reachable for pybo to spawn its daemon: an
# installed `bo` on PATH (this script's first step) is enough.
set -eu

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DO_PYBO=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --pybo) DO_PYBO=1 ;;
    --pybo=*) PY_TARGET="$1"; DO_PYBO=1 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

echo "== installing the bo binary (cargo install --path $ROOT) =="
cargo install --path "$ROOT" --locked
echo "== bo installed: $(command -v bo)"

if [ "$DO_PYBO" = 1 ]; then
  if ! command -v uv >/dev/null 2>&1; then
    echo "error: uv is required to build pybo" >&2
    exit 1
  fi
  if [ -n "${PY_TARGET:-}" ]; then
    PY="$(printf '%s' "${PY_TARGET#--pybo=}")"
  elif [ -n "${BO_PYTHON:-}" ]; then
    PY="$BO_PYTHON"
  elif [ -n "${VIRTUAL_ENV:-}" ]; then
    PY="$VIRTUAL_ENV/bin/python"
  else
    PY="$(command -v python3)"
  fi

  echo "== building the pybo wheel =="
  ( cd "$ROOT/py" && uv build --wheel )

  WHEEL="$(ls "$ROOT/py/dist"/pybo-*.whl | head -1)"
  echo "== installing $WHEEL into $PY =="
  PIP_BREAK=()
  if [ "${BO_PIP_BREAK:-0}" = 1 ]; then
    PIP_BREAK=(--break-system-packages)
  fi
  "$PY" -m pip install --force-reinstall --no-deps "${PIP_BREAK[@]+"${PIP_BREAK[@]}"}" "$WHEEL"
  "$PY" -c "import pybo; print('== pybo', pybo.version(), 'importable from', '$PY')"
fi

echo "== done. Try: bo --help  /  python3 -c 'import pybo' =="

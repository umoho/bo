#!/usr/bin/env bash
# Install bo for this machine: the `bo` CLI/daemon binary system-wide
# (cargo install) and the `pybo` Python package.
#
# Usage:
#   ./scripts/install.sh
#
# pybo is built as one abi3 wheel (uv) and installed into the Python of
# your choice: $BO_PYTHON if set, else the active virtualenv (VIRTUAL_ENV),
# else `python3` on PATH. Set BO_PIP_BREAK=1 to add --break-system-packages
# when the target interpreter refuses system installs (PEP 668).
#
# The bo binary must be reachable for pybo to spawn its daemon: an
# installed `bo` on PATH (installed here first) is enough.
# Uninstall with: ./scripts/uninstall.sh
set -eu

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "== [1/2] installing the bo binary (cargo install --path $ROOT) =="
cargo install --path "$ROOT" --locked
echo "== bo installed: $(command -v bo)"

echo "== [2/2] installing pybo =="
if ! command -v uv >/dev/null 2>&1; then
  echo "error: uv is required to build pybo" >&2
  exit 1
fi
if [ -n "${BO_PYTHON:-}" ]; then
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

echo "== done. Try: bo --help  /  $PY -c 'import pybo'  —  uninstall: ./scripts/uninstall.sh =="

#!/bin/zsh
set -euo pipefail

RESOURCE_DIR="$1"
SUPPORT_DIR="$HOME/Library/Application Support/Snack Record"
RUNTIME_DIR="$SUPPORT_DIR/Runtime"
MODELS_DIR="$SUPPORT_DIR/Models"
VENV_DIR="$RUNTIME_DIR/venv"
ARCH="$(uname -m)"

python_is_compatible() {
  "$1" - "$ARCH" <<'PY' >/dev/null 2>&1
import platform
import sys
version = sys.version_info[:2]
supported = version >= (3, 10) and platform.machine() == sys.argv[1]
if sys.argv[1] == "x86_64":
    supported = supported and version <= (3, 12)
raise SystemExit(not supported)
PY
}

find_python() {
  local candidate executable
  for candidate in python3.11 python3.12 python3.10 python3.13 python3; do
    executable="$(command -v "$candidate" 2>/dev/null || true)"
    if [[ -n "$executable" ]] && python_is_compatible "$executable"; then
      print -r -- "$executable"
      return 0
    fi
  done
  return 1
}

PYTHON_BIN="$(find_python || true)"
[[ -n "$PYTHON_BIN" ]] || { echo "请先安装原生架构的 Python 3.10 或更高版本" >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "请先安装 FFmpeg，再安装本地转写资源" >&2; exit 1; }
if [[ -x "$VENV_DIR/bin/python" ]] && ! python_is_compatible "$VENV_DIR/bin/python"; then
  echo "现有 Snack Record Python 环境与当前设备架构不兼容" >&2
  exit 1
fi
mkdir -p "$RUNTIME_DIR" "$MODELS_DIR"
[[ -x "$VENV_DIR/bin/python" ]] || "$PYTHON_BIN" -m venv "$VENV_DIR"
"$VENV_DIR/bin/python" -m pip install --upgrade pip
"$VENV_DIR/bin/python" -m pip install -r "$RESOURCE_DIR/recording-requirements.txt"
MODELSCOPE_CACHE="$MODELS_DIR" "$VENV_DIR/bin/python" "$RESOURCE_DIR/download_models.py"

#!/bin/zsh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
APP_SUPPORT="$HOME/Library/Application Support/Snack Record"
RUNTIME_DIR="$APP_SUPPORT/Runtime"
MODELS_DIR="$APP_SUPPORT/Models"
VENV_DIR="$RUNTIME_DIR/venv"
ARCH="$(uname -m)"

python_is_compatible() {
  local executable="$1"
  "$executable" - "$ARCH" <<'PY' >/dev/null 2>&1
import platform
import sys

expected_arch = sys.argv[1]
version = sys.version_info[:2]
supported_version = version >= (3, 10)
if expected_arch == "x86_64":
    supported_version = supported_version and version <= (3, 12)

raise SystemExit(not (platform.machine() == expected_arch and supported_version))
PY
}

find_compatible_python() {
  local candidate executable
  local -a candidates
  if [[ "$ARCH" == "x86_64" ]]; then
    candidates=(python3.11 python3.12 python3.10 python3)
  else
    candidates=(python3 python3.13 python3.12 python3.11 python3.10)
  fi

  for candidate in "${candidates[@]}"; do
    executable="$(command -v "$candidate" 2>/dev/null || true)"
    if [[ -n "$executable" ]] && python_is_compatible "$executable"; then
      print -r -- "$executable"
      return 0
    fi
  done
  return 1
}

install_python_if_needed() {
  local python_bin
  python_bin="$(find_compatible_python || true)"
  if [[ -n "$python_bin" ]]; then
    print -r -- "$python_bin"
    return 0
  fi
  command -v brew >/dev/null || {
    echo "A compatible Python is required. Install Homebrew first." >&2
    return 1
  }
  brew install python@3.11
  print -r -- "$(brew --prefix python@3.11)/bin/python3.11"
}

install_ffmpeg_if_needed() {
  if command -v ffmpeg >/dev/null; then
    return 0
  fi
  command -v brew >/dev/null || {
    echo "FFmpeg is required. Install Homebrew first." >&2
    return 1
  }
  brew install ffmpeg
}

prepare_python_runtime() {
  local python_bin="$1"
  mkdir -p "$RUNTIME_DIR" "$MODELS_DIR"
  if [[ -x "$VENV_DIR/bin/python" ]] && ! python_is_compatible "$VENV_DIR/bin/python"; then
    rm -rf "$VENV_DIR"
  fi
  if [[ ! -x "$VENV_DIR/bin/python" ]]; then
    "$python_bin" -m venv "$VENV_DIR"
  fi
  "$VENV_DIR/bin/python" -m pip install --upgrade pip
  "$VENV_DIR/bin/python" -m pip install -r "$ROOT/requirements.txt"
}

download_models() {
  MODELSCOPE_CACHE="$MODELS_DIR" \
    "$VENV_DIR/bin/python" "$ROOT/scripts/download_models.py"
}

main() {
  case "$ARCH" in
    arm64|x86_64) ;;
    *) echo "Unsupported Mac architecture: $ARCH" >&2; return 1 ;;
  esac

  local python_bin
  python_bin="$(install_python_if_needed)"
  python_is_compatible "$python_bin" || {
    echo "Python runtime architecture or version is incompatible." >&2
    return 1
  }
  install_ffmpeg_if_needed
  prepare_python_runtime "$python_bin"
  download_models
  echo "Snack Record runtime is ready."
}

main "$@"

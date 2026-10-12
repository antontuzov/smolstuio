#!/usr/bin/env bash
# Install both CLIs from this checkout — `smoll`, the coding agent, and `smollm`,
# the model CLI — and, when a bundle exists, the desktop app. Everything is
# local: no release download, no sudo, no telemetry.
#
#   ./scripts/install.sh            # both CLIs into ~/.local/bin
#   ./scripts/install.sh --app      # also copy the .app to /Applications
#   ./scripts/install.sh --prefix /usr/local
set -euo pipefail

PREFIX="${HOME}/.local"
WITH_APP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app) WITH_APP=1; shift ;;
    --prefix)
      [[ $# -ge 2 ]] || { echo "install.sh: --prefix needs a value" >&2; exit 2; }
      PREFIX="$2"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown option '$1' (try --help)" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${PREFIX}/bin"

command -v cargo >/dev/null 2>&1 || {
  echo "install.sh: cargo not found. Install Rust 1.77+ from https://rustup.rs and retry." >&2
  exit 1
}

echo "==> Building smollm (release)"
cargo build --release --manifest-path "${ROOT}/Cargo.toml" -p smollm-cli --bin smollm

# `openai-compatible` is the one feature this ships with everywhere, and the one
# the release archives carry: a build without it answers from `mock` or `echo`
# only, and an agent running a scripted model in place of yours is a worse
# surprise at install time than a slower build. Weights still need a `llama-cpp`
# build, which this script does not pretend to promise — see docs/release.md.
echo "==> Building smoll (release, openai-compatible)"
cargo build --release --manifest-path "${ROOT}/Cargo.toml" -p agent-cli --bin smoll \
  --features openai-compatible

echo "==> Installing to ${BIN}"
mkdir -p "${BIN}"
# Install rather than move: the checkout keeps its build tree.
install -m 0755 "${ROOT}/target/release/smollm" "${BIN}/smollm"
install -m 0755 "${ROOT}/target/release/smoll" "${BIN}/smoll"

if [[ "${WITH_APP}" -eq 1 ]]; then
  APP="$(find "${ROOT}/desktop/src-tauri/target/release/bundle/macos" -maxdepth 1 -name '*.app' 2>/dev/null | head -1 || true)"
  if [[ -z "${APP}" ]]; then
    echo "install.sh: no .app bundle found. Build it first: (cd desktop && pnpm tauri build)" >&2
    exit 1
  fi
  DEST="/Applications"
  [[ -w "${DEST}" ]] || { echo "install.sh: ${DEST} is not writable. Copy ${APP} there yourself." >&2; exit 1; }
  echo "==> Copying $(basename "${APP}") to ${DEST}"
  rm -rf "${DEST}/$(basename "${APP}")"
  cp -R "${APP}" "${DEST}/"
fi

case ":${PATH}:" in
  *":${BIN}:"*) ;;
  *) printf '\nAdd %s to your PATH, then restart your shell:\n\n  export PATH="%s:$PATH"\n\n' "${BIN}" "${BIN}" ;;
esac

echo
"${BIN}/smollm" --version
"${BIN}/smoll" --version
echo "smollm is installed. Try: smollm hardware && smollm doctor"
echo "smoll is installed. Try: cd a repository && smoll doctor && smoll tools"

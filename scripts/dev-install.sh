#!/usr/bin/env bash
# dev-install.sh — build derrick from source and install locally
# Usage: scripts/dev-install.sh [--prefix ~/.local]
set -euo pipefail

BIN="derrick"
PREFIX="/usr/local/bin"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix|-p) PREFIX="$2"; shift 2 ;;
    *)           echo "usage: $0 [--prefix dir]" >&2; exit 1 ;;
  esac
done

PREFIX="${DERRICK_PREFIX:-$PREFIX}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# colour support
if [ -t 1 ]; then
  GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BOLD='\033[1m'; RESET='\033[0m'
else
  GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi

info()  { echo -e "${BOLD}[derrick]${RESET} $*" >&2; }
ok()    { echo -e "${GREEN}[derrick]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[derrick]${RESET} $*"; }
die()   { echo -e "${BOLD}[derrick] error:${RESET} $*" >&2; exit 1; }

# ── prerequisites ───────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed."; }
need cargo
need rustc

info "Building derrick from source (release profile)…"
cargo build --release --manifest-path "${REPO_ROOT}/Cargo.toml"

SRC="${REPO_ROOT}/target/release/${BIN}"
if [ ! -f "$SRC" ]; then
  die "Build succeeded but binary not found at ${SRC}"
fi

mkdir -p "$PREFIX"

if [ -w "$PREFIX" ]; then
  cp "$SRC" "${PREFIX}/${BIN}"
else
  info "Installing to ${PREFIX} (may prompt for password)…"
  sudo cp "$SRC" "${PREFIX}/${BIN}"
fi

ok "derrick $("${PREFIX}/${BIN}" --version) installed to ${PREFIX}/${BIN}"

info "Build artifact cached at ${SRC} (rerun to update)"
info "Uninstall: rm -f ${PREFIX}/${BIN}"

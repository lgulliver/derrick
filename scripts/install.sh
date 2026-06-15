#!/usr/bin/env bash
# derrick installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/lgulliver/derrick/main/scripts/install.sh | bash -s -- --nightly
set -euo pipefail

REPO="lgulliver/derrick"
BIN="derrick"
INSTALL_DIR="${DERRICK_INSTALL_DIR:-/usr/local/bin}"
NIGHTLY=false

# ── colours ────────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  BOLD='\033[1m'; RESET='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi

info()  { echo -e "${BOLD}[derrick]${RESET} $*" >&2; }
ok()    { echo -e "${GREEN}[derrick]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[derrick]${RESET} $*"; }
die()   { echo -e "${RED}[derrick] error:${RESET} $*" >&2; exit 1; }

# ── argument parsing ───────────────────────────────────────────────────────────
for arg in "$@"; do
  case "$arg" in
    --nightly) NIGHTLY=true ;;
    *) die "Unknown argument: $arg" ;;
  esac
done

# ── platform detection ─────────────────────────────────────────────────────────
detect_platform() {
  local os arch

  case "$(uname -s)" in
    Linux)  os="linux"  ;;
    Darwin) os="macos"  ;;
    *)      die "Unsupported OS: $(uname -s). Windows support is planned for v1.1." ;;
  esac

  case "$(uname -m)" in
    x86_64 | amd64)   arch="x86_64" ;;
    arm64  | aarch64) arch="arm64"  ;;
    *) die "Unsupported architecture: $(uname -m)" ;;
  esac

  echo "${os}-${arch}"
}

# ── dependency check ───────────────────────────────────────────────────────────
need() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed."
}

need curl

# Prefer shasum (macOS, perl) and fall back to sha256sum (coreutils, the
# common tool on minimal Linux images). Die only if neither exists.
if command -v shasum >/dev/null 2>&1; then
  sha256_verify() { shasum -a 256 -c "$1"; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_verify() { sha256sum -c "$1"; }
else
  die "'shasum' or 'sha256sum' is required but neither is installed."
fi

# ── resolve latest stable version ─────────────────────────────────────────────
resolve_stable() {
  local version="${DERRICK_VERSION:-}"
  if [ -n "$version" ]; then
    echo "$version"
    return
  fi

  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | grep '"tag_name"' \
    | sed 's/.*"tag_name": *"\(.*\)".*/\1/' || true
}

# ── resolve latest nightly version ────────────────────────────────────────────
resolve_nightly() {
  curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=100" 2>/dev/null \
    | grep '"tag_name"' \
    | sed 's/.*"tag_name": *"\(.*\)".*/\1/' \
    | grep '^nightly-' \
    | head -1 || true
}

# ── main ───────────────────────────────────────────────────────────────────────
main() {
  local platform version artifact base_url tmp_dir channel

  platform=$(detect_platform)

  if [ "$NIGHTLY" = true ]; then
    channel="nightly"
    info "Fetching latest nightly release tag…"
    version=$(resolve_nightly)
    [ -n "$version" ] || die "No nightly release found. Check https://github.com/${REPO}/releases"
  else
    channel="stable"
    info "Fetching latest stable release tag…"
    version=$(resolve_stable)
    [ -n "$version" ] || die "No stable release found. Use --nightly for the latest nightly build, or set DERRICK_VERSION=vX.Y.Z to pin a version."
  fi

  artifact="${BIN}-${platform}"
  base_url="https://github.com/${REPO}/releases/download/${version}"
  tmp_dir=$(mktemp -d)
  trap 'rm -rf "${tmp_dir}"' EXIT

  info "Installing ${BOLD}derrick ${version}${RESET} (${channel}) for ${platform}…"

  # Download binary + checksum
  info "Downloading ${artifact}…"
  curl -fsSL "${base_url}/${artifact}" -o "${tmp_dir}/${artifact}" \
    || die "Failed to download ${base_url}/${artifact}"
  curl -fsSL "${base_url}/${artifact}.sha256" -o "${tmp_dir}/${artifact}.sha256" \
    || die "Failed to download checksum ${base_url}/${artifact}.sha256 — refusing to install an unverified binary."
  [ -s "${tmp_dir}/${artifact}.sha256" ] \
    || die "Checksum file is empty — refusing to install an unverified binary."

  # Verify checksum
  info "Verifying checksum…"
  pushd "${tmp_dir}" >/dev/null
  sha256_verify "${artifact}.sha256" >/dev/null || die "Checksum verification failed."
  popd >/dev/null

  # Install
  chmod +x "${tmp_dir}/${artifact}"

  if [ -w "$INSTALL_DIR" ]; then
    mv "${tmp_dir}/${artifact}" "${INSTALL_DIR}/${BIN}"
  else
    info "Installing to ${INSTALL_DIR} (may prompt for password)…"
    sudo mv "${tmp_dir}/${artifact}" "${INSTALL_DIR}/${BIN}"
  fi

  # Verify
  if command -v derrick >/dev/null 2>&1; then
    ok "derrick $(derrick --version) installed to $(command -v derrick)"
  else
    warn "Installed to ${INSTALL_DIR}/${BIN} but it's not on your PATH."
    warn "Add this to your shell profile:"
    warn "  export PATH=\"${INSTALL_DIR}:\$PATH\""
  fi

  if [ "$NIGHTLY" = true ]; then
    warn "You are running a nightly build. Expect rough edges."
    warn "To switch back to stable: curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/install.sh | bash"
  fi

  echo ""
  info "Get started:"
  echo "  cd ~/repos/my-project"
  echo "  derrick init"
  echo "  derrick drill \"describe your feature\""
  echo ""
  info "Docs: https://github.com/${REPO}#readme"
}

main "$@"

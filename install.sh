#!/usr/bin/env bash
#
# dictator installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/reddb-io/dictator/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/reddb-io/dictator/main/install.sh | bash -s -- --version v0.1.0
#   curl -fsSL https://raw.githubusercontent.com/reddb-io/dictator/main/install.sh | bash -s -- --install-dir /usr/local/bin
#
set -euo pipefail

REPO="reddb-io/dictator"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
BINARY_NAME="dictator"
VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)      VERSION="$2"; shift 2 ;;
    --install-dir)  INSTALL_DIR="$2"; shift 2 ;;
    -h|--help)
      cat <<EOF
dictator installer

Usage: install.sh [OPTIONS]

Options:
  --version <vX.Y.Z>     Install a specific release (default: latest)
  --install-dir <path>   Install location (default: ~/.local/bin)
  -h, --help             Show this help
EOF
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

say()  { printf '\033[1;36m›\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# --- pick a downloader ------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  dl()      { curl -fsSL "$1"; }
  dl_to()   { curl -fL  -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl()      { wget -qO- "$1"; }
  dl_to()   { wget -qO  "$2" "$1"; }
else
  die "curl or wget is required"
fi

# --- detect platform → matches the release asset names ----------------------
detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux*)               OS="linux" ;;
    Darwin*)              OS="macos" ;;
    MINGW*|MSYS*|CYGWIN*) OS="windows" ;;
    *) die "Unsupported OS: $os" ;;
  esac

  case "$arch" in
    x86_64|amd64)   ARCH="x86_64" ;;
    aarch64|arm64)  ARCH="aarch64" ;;
    *) die "Unsupported architecture: $arch (prebuilt binaries cover x86_64 and aarch64)" ;;
  esac

  PLATFORM="${OS}-${ARCH}"
  EXT=""
  [[ "$OS" == "windows" ]] && EXT=".exe"
}

# --- resolve the release tag ------------------------------------------------
resolve_tag() {
  if [[ -n "$VERSION" ]]; then
    RELEASE_TAG="$VERSION"
    return
  fi
  local api="https://api.github.com/repos/$REPO/releases/latest"
  local json
  json="$(dl "$api" || true)"
  [[ -n "$json" ]] || die "Could not reach the GitHub API to find the latest release"
  RELEASE_TAG="$(printf '%s' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  [[ -n "$RELEASE_TAG" ]] || die "No published release found for $REPO"
}

# --- verify the .sha256 sidecar (skips silently if absent) ------------------
verify_checksum() {
  local file="$1" asset="$2"
  local sums expected actual
  sums="$(dl "https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${asset}.sha256" 2>/dev/null || true)"
  expected="$(printf '%s' "$sums" | awk '{print $1}' | tr -d '[:space:]')"
  [[ -n "$expected" ]] || { warn "no checksum published; skipping verification"; return 0; }

  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else
    warn "no sha256 tool; skipping verification"; return 0
  fi

  [[ "$expected" == "$actual" ]] || die "Checksum mismatch for ${asset} (expected ${expected}, got ${actual})"
  say "checksum OK"
}

main() {
  detect_platform
  resolve_tag

  local asset="${BINARY_NAME}-${PLATFORM}${EXT}"
  local url="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${asset}"
  local tmp
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT

  say "installing ${BINARY_NAME} ${RELEASE_TAG} (${PLATFORM})"
  dl_to "$url" "$tmp" || die "Download failed: $url
This platform may not have a prebuilt binary — build from source instead (see the README)."

  verify_checksum "$tmp" "$asset"

  mkdir -p "$INSTALL_DIR"
  local dest="${INSTALL_DIR}/${BINARY_NAME}${EXT}"
  mv "$tmp" "$dest"
  trap - EXIT
  [[ "$OS" != "windows" ]] && chmod +x "$dest"
  say "installed → ${dest}"

  # PATH hint
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) warn "${INSTALL_DIR} is not on your PATH. Add it, e.g.:
      echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile && source ~/.profile" ;;
  esac

  # Linux runtime libraries the dynamically-linked binary needs.
  if [[ "$OS" == "linux" ]]; then
    warn "Linux runtime libs required: libasound2, libxdo3, libxtst6, libxi6, libdbus-1-3 (Debian/Ubuntu):
      sudo apt-get install -y libasound2 libxdo3 libxtst6 libxi6 libdbus-1-3"
  fi

  printf '\n\033[1;32m✓ done\033[0m\n'
  echo "Next:"
  echo "  echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.dictator.env"
  echo "  ${BINARY_NAME} --help     # press F9 to start/stop dictation"
}

main

#!/usr/bin/env bash
#
# dit installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/reddb-io/dit/main/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --yes            # non-interactive, accept defaults
#   curl -fsSL .../install.sh | bash -s -- --api-key sk_... --with-service
#
# Flags:
#   --version <vX.Y.Z>     install a specific release (default: latest)
#   --install-dir <path>   install location (default: ~/.local/bin)
#   --api-key <key>        write this ElevenLabs key to ~/.dit.env
#   --with-service         install the autostart user service
#   --no-service           never install the service
#   --skip-deps            don't touch system runtime libraries
#   --yes, -y              non-interactive: accept defaults, no prompts
#   -h, --help             show this help
#
set -euo pipefail

REPO="reddb-io/dit"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
BINARY_NAME="dit"
VERSION=""
API_KEY=""
ASSUME_YES=false
WANT_SERVICE="ask"   # ask | yes | no
SKIP_DEPS=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)      VERSION="$2"; shift 2 ;;
    --install-dir)  INSTALL_DIR="$2"; shift 2 ;;
    --api-key)      API_KEY="$2"; shift 2 ;;
    --with-service) WANT_SERVICE="yes"; shift ;;
    --no-service)   WANT_SERVICE="no"; shift ;;
    --skip-deps)    SKIP_DEPS=true; shift ;;
    -y|--yes)       ASSUME_YES=true; shift ;;
    -h|--help)      sed -n '2,20p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

say()  { printf '\033[1;36m›\033[0m %b\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %b\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %b\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m✓\033[0m %b\n' "$*"; }

# Interactive only when a real terminal is reachable and --yes wasn't passed.
# Under `curl | bash` stdin is the script, so we read from /dev/tty.
interactive() { [[ "$ASSUME_YES" == false && -r /dev/tty ]]; }
ask() { # ask "Question?" "default(Y/n)"  → echoes the answer
  local prompt="$1" def="${2:-}"
  if ! interactive; then echo "$def"; return; fi
  local ans=""
  printf '\033[1;35m?\033[0m %s ' "$prompt" > /dev/tty
  read -r ans < /dev/tty || ans=""
  echo "${ans:-$def}"
}
confirm() { # confirm "Question?" default(yes/no) → return 0 if yes
  local def="${2:-yes}" hint="[Y/n]"
  [[ "$def" == no ]] && hint="[y/N]"
  if ! interactive; then [[ "$def" == yes ]]; return; fi
  local a; a="$(ask "$1 $hint" "$def")"
  case "${a,,}" in
    y|yes) return 0 ;;
    n|no)  return 1 ;;
    *)     [[ "$def" == yes ]] ;;
  esac
}

# --- pick a downloader ------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
  dl()    { curl -fsSL "$1"; }
  dl_to() { curl -fL  -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl()    { wget -qO- "$1"; }
  dl_to() { wget -qO  "$2" "$1"; }
else
  die "curl or wget is required"
fi

# --- detect platform → matches the release asset names ----------------------
detect_platform() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Linux*)               OS="linux" ;;
    Darwin*)              OS="macos" ;;
    MINGW*|MSYS*|CYGWIN*) OS="windows" ;;
    *) die "Unsupported OS: $os" ;;
  esac
  case "$arch" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) die "Unsupported architecture: $arch (prebuilt binaries cover x86_64 and aarch64)" ;;
  esac
  PLATFORM="${OS}-${ARCH}"
  EXT=""
  if [[ "$OS" == "windows" ]]; then EXT=".exe"; fi
}

resolve_tag() {
  if [[ -n "$VERSION" ]]; then RELEASE_TAG="$VERSION"; return; fi
  local json; json="$(dl "https://api.github.com/repos/$REPO/releases/latest" || true)"
  [[ -n "$json" ]] || die "Could not reach the GitHub API to find the latest release"
  RELEASE_TAG="$(printf '%s' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  [[ -n "$RELEASE_TAG" ]] || die "No published release found for $REPO"
}

verify_checksum() {
  local file="$1" asset="$2" sums expected actual
  sums="$(dl "https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${asset}.sha256" 2>/dev/null || true)"
  expected="$(printf '%s' "$sums" | awk '{print $1}' | tr -d '[:space:]')"
  [[ -n "$expected" ]] || { warn "no checksum published; skipping verification"; return 0; }
  if command -v sha256sum >/dev/null 2>&1; then actual="$(sha256sum "$file" | awk '{print $1}')"
  elif command -v shasum    >/dev/null 2>&1; then actual="$(shasum -a 256 "$file" | awk '{print $1}')"
  else warn "no sha256 tool; skipping verification"; return 0; fi
  [[ "$expected" == "$actual" ]] || die "Checksum mismatch for ${asset}"
  ok "checksum verified"
}

# --- Linux runtime dependencies --------------------------------------------
linux_deps() {
  [[ "$OS" == "linux" && "$SKIP_DEPS" == false ]] || return 0

  # sonames the dynamically-linked binary needs (audio + X11 input).
  local need=(libasound.so.2 libxdo.so.3 libXtst.so.6 libXi.so.6)
  local missing=()
  if command -v ldconfig >/dev/null 2>&1; then
    local cache; cache="$(ldconfig -p 2>/dev/null || true)"
    for so in "${need[@]}"; do
      printf '%s' "$cache" | grep -q "$so" || missing+=("$so")
    done
  fi
  [[ ${#missing[@]} -gt 0 ]] || { ok "runtime libraries present"; return 0; }

  warn "missing runtime libraries: ${missing[*]}"
  local pm="" cmd=""
  if   command -v apt-get >/dev/null 2>&1; then pm=apt;    cmd="sudo apt-get install -y libasound2 libxdo3 libxtst6 libxi6"
  elif command -v dnf     >/dev/null 2>&1; then pm=dnf;    cmd="sudo dnf install -y alsa-lib libxdo libXtst libXi"
  elif command -v pacman  >/dev/null 2>&1; then pm=pacman; cmd="sudo pacman -S --needed --noconfirm alsa-lib xdotool libxtst libxi"
  elif command -v zypper  >/dev/null 2>&1; then pm=zypper; cmd="sudo zypper install -y libasound2 libxdo3 libXtst6 libXi6"
  else warn "unknown package manager — install the equivalents of: ${need[*]}"; return 0; fi

  if [[ "$ASSUME_YES" == true ]] || confirm "Install them now with $pm?" yes; then
    say "running: $cmd"
    eval "$cmd" || warn "dependency install failed — run it manually:\n  $cmd"
  else
    warn "skipped — install later with:\n  $cmd"
  fi
}

# --- API key ----------------------------------------------------------------
setup_api_key() {
  local env_file="$HOME/.dit.env"
  if [[ -n "$API_KEY" ]]; then
    umask 177; printf 'ELEVENLABS_API_KEY=%s\n' "$API_KEY" > "$env_file"
    ok "wrote API key to $env_file"
    return
  fi
  # Already configured?
  if [[ -f "$env_file" ]] && grep -q '^ELEVENLABS_API_KEY=..' "$env_file" 2>/dev/null; then
    ok "API key already set in $env_file"
    return
  fi
  if interactive; then
    local key; key="$(ask "Paste your ElevenLabs API key (blank to skip):" "")"
    if [[ -n "$key" ]]; then
      umask 177; printf 'ELEVENLABS_API_KEY=%s\n' "$key" > "$env_file"
      ok "wrote API key to $env_file"
      return
    fi
  fi
  warn "no API key set yet — add it later:\n  echo 'ELEVENLABS_API_KEY=sk_your_key' > $env_file"
}

main() {
  detect_platform
  resolve_tag

  local asset="${BINARY_NAME}-${PLATFORM}${EXT}"
  local url="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${asset}"
  local tmp; tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT

  say "installing ${BINARY_NAME} ${RELEASE_TAG} (${PLATFORM})"
  dl_to "$url" "$tmp" || die "Download failed: $url
This platform may not have a prebuilt binary — build from source instead (see the README)."
  verify_checksum "$tmp" "$asset"

  mkdir -p "$INSTALL_DIR"
  local dest="${INSTALL_DIR}/${BINARY_NAME}${EXT}"
  mv "$tmp" "$dest"; trap - EXIT
  if [[ "$OS" != "windows" ]]; then chmod +x "$dest"; fi
  # macOS: clear the Gatekeeper quarantine flag so it runs without a prompt.
  if [[ "$OS" == "macos" ]]; then xattr -d com.apple.quarantine "$dest" 2>/dev/null || true; fi
  ok "installed → ${dest}"

  # PATH hint
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) warn "${INSTALL_DIR} is not on your PATH. Add it, e.g.:\n      echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile && source ~/.profile" ;;
  esac

  linux_deps
  setup_api_key

  # Smoke test (skip cross-OS shells where it can't run, e.g. windows under msys).
  if [[ "$OS" != "windows" ]] && "$dest" --version >/dev/null 2>&1; then
    ok "$("$dest" --version) runs"
  elif [[ "$OS" != "windows" ]]; then
    warn "the binary did not run — likely a missing runtime library (see above)"
  fi

  # Autostart service
  if [[ "$OS" != "windows" || "$WANT_SERVICE" == "yes" ]]; then
    local do_service=false
    case "$WANT_SERVICE" in
      yes) do_service=true ;;
      no)  do_service=false ;;
      ask) confirm "Start dit automatically at login (install the user service)?" no && do_service=true || true ;;
    esac
    if [[ "$do_service" == true ]]; then
      "$dest" service install && ok "autostart service installed" || warn "could not install the service"
    fi
  fi

  printf '\n'
  ok "done"
  echo "Press F9 to start/stop dictation. Run '${BINARY_NAME} --help' for options."
}

main

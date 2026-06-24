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
#   --force                reinstall even if the requested version is already present
#   --static               force the fully-static (musl) build, even on a glibc host
#   --check-only           report whether an update is available, then exit
#   --yes, -y              non-interactive: accept defaults, no prompts
#   -h, --help             show this help
#
# Linux binaries are portable across distro versions. The default asset is built
# against an old glibc floor (>= 2.28 — every Ubuntu since 18.04, Debian 10+),
# so a single binary runs on 20.04/22.04/24.04/26.04 alike. When the host glibc
# is older than that, or absent (a musl distro like Alpine), the installer
# automatically falls back to the fully-static `-static` build.
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
FORCE=false
FORCE_STATIC=false
CHECK_ONLY=false

# Lowest glibc version the default (non-static) Linux assets are built against.
# Must match the zigbuild target floor in .github/workflows/release.yml.
GLIBC_FLOOR="2.28"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)      VERSION="$2"; shift 2 ;;
    --install-dir)  INSTALL_DIR="$2"; shift 2 ;;
    --api-key)      API_KEY="$2"; shift 2 ;;
    --with-service) WANT_SERVICE="yes"; shift ;;
    --no-service)   WANT_SERVICE="no"; shift ;;
    --skip-deps)    SKIP_DEPS=true; shift ;;
    --force)        FORCE=true; shift ;;
    --static)       FORCE_STATIC=true; shift ;;
    --check-only)   CHECK_ONLY=true; shift ;;
    -y|--yes)       ASSUME_YES=true; shift ;;
    -h|--help)      sed -n '2,27p' "$0" 2>/dev/null || true; exit 0 ;;
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
  dl()      { curl -fsSL "$1"; }
  dl_to()   { curl -fL  -o "$2" "$1"; }
  dl_head() { curl -fsSIL -o /dev/null "$1"; }   # success iff the URL exists
elif command -v wget >/dev/null 2>&1; then
  dl()      { wget -qO- "$1"; }
  dl_to()   { wget -qO  "$2" "$1"; }
  dl_head() { wget -q --spider "$1"; }
else
  die "curl or wget is required"
fi

# Does a release asset exist for the resolved tag?
asset_exists() {
  dl_head "https://github.com/${REPO}/releases/download/${RELEASE_TAG}/$1" 2>/dev/null
}

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
    x86_64|amd64)        ARCH="x86_64" ;;
    aarch64|arm64)       ARCH="aarch64" ;;
    armv7l|armv7|armhf)  ARCH="armv7" ;;
    *) die "Unsupported architecture: $arch (prebuilt binaries cover x86_64, aarch64 and armv7)" ;;
  esac
  PLATFORM="${OS}-${ARCH}"
  EXT=""
  if [[ "$OS" == "windows" ]]; then EXT=".exe"; fi
}

# Detect the host glibc version (e.g. "2.35"), or empty on a musl/non-glibc host.
detect_glibc() {
  local v=""
  # ldd --version is the most portable probe; musl prints "musl libc" instead.
  if command -v ldd >/dev/null 2>&1; then
    local out; out="$(ldd --version 2>&1 | head -1)"
    if printf '%s' "$out" | grep -qi musl; then printf ''; return; fi
    v="$(printf '%s' "$out" | grep -oE '[0-9]+\.[0-9]+' | head -1)"
  fi
  if [[ -z "$v" ]] && command -v getconf >/dev/null 2>&1; then
    v="$(getconf GNU_LIBC_VERSION 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -1)"
  fi
  printf '%s' "$v"
}

# True (0) when version $1 is strictly older than $2.
version_lt() {
  [[ "$1" == "$2" ]] && return 1
  local first; first="$(printf '%s\n%s\n' "$1" "$2" | sort -V 2>/dev/null | head -n1)"
  [[ "$first" == "$1" ]]
}

# Pick the best Linux asset: the portable glibc build by default, the
# fully-static (musl) build when the host glibc is too old / absent / forced.
# Sets ASSET and VARIANT. macOS and Windows ship a single asset per platform.
choose_asset() {
  local base="${BINARY_NAME}-${PLATFORM}${EXT}"
  if [[ "$OS" != "linux" ]]; then ASSET="$base"; VARIANT="native"; return; fi

  local static="${BINARY_NAME}-${PLATFORM}-static"
  local glibc; glibc="$(detect_glibc)"
  local want_static=false reason=""
  if [[ "$FORCE_STATIC" == true ]]; then
    want_static=true; reason="forced with --static"
  elif [[ -z "$glibc" ]]; then
    want_static=true; reason="no glibc detected (musl host?)"
  elif version_lt "$glibc" "$GLIBC_FLOOR"; then
    want_static=true; reason="host glibc ${glibc} < ${GLIBC_FLOOR}"
  fi

  if [[ "$want_static" == true ]]; then
    if asset_exists "$static"; then
      say "selecting the static build (${reason})"
      ASSET="$static"; VARIANT="static"; return
    fi
    warn "no static build published for ${PLATFORM}; falling back to the glibc build"
    ASSET="$base"; VARIANT="glibc"; return
  fi

  if asset_exists "$base"; then ASSET="$base"; VARIANT="glibc"; return; fi
  if asset_exists "$static"; then
    warn "no glibc build for ${PLATFORM}; using the static build"
    ASSET="$static"; VARIANT="static"; return
  fi
  ASSET="$base"; VARIANT="glibc"   # let the download step report the 404
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

normalize_version() {
  local v="${1:-}"
  v="${v#dit }"
  v="${v#v}"
  printf '%s' "$v"
}

version_of() {
  local bin="$1" out=""
  [[ -x "$bin" || "$OS" == "windows" ]] || return 1
  out="$($bin --version 2>/dev/null || true)"
  [[ -n "$out" ]] || return 1
  printf '%s' "$(normalize_version "$out")"
}

find_existing_binary() {
  local dest="$1" found=""
  if [[ -f "$dest" ]]; then
    printf '%s' "$dest"
    return 0
  fi
  found="$(command -v "${BINARY_NAME}${EXT}" 2>/dev/null || command -v "$BINARY_NAME" 2>/dev/null || true)"
  [[ -n "$found" ]] || return 1
  printf '%s' "$found"
}

compare_versions() {
  # Prints -1, 0, or 1 for a < b, a == b, or a > b. Uses sort -V where
  # available and falls back to equality-only comparison on minimal systems.
  local a b first
  a="$(normalize_version "$1")"
  b="$(normalize_version "$2")"
  [[ "$a" == "$b" ]] && { echo 0; return; }
  if first="$(printf '%s\n%s\n' "$a" "$b" | sort -V 2>/dev/null | head -n1)"; then
    [[ "$first" == "$a" ]] && echo -1 || echo 1
  else
    echo 1
  fi
}

maybe_report_existing() {
  local dest="$1" target_version existing existing_ver cmp
  target_version="$(normalize_version "$RELEASE_TAG")"
  existing="$(find_existing_binary "$dest" || true)"
  [[ -n "$existing" ]] || return 0

  if existing_ver="$(version_of "$existing" || true)" && [[ -n "$existing_ver" ]]; then
    cmp="$(compare_versions "$existing_ver" "$target_version")"
    if [[ "$cmp" == "0" ]]; then
      ok "found existing ${BINARY_NAME} ${existing_ver} at ${existing}"
      if [[ "$FORCE" == false ]]; then
        say "already on ${RELEASE_TAG}; use --force to reinstall the binary"
        ALREADY_CURRENT=true
      fi
    elif [[ "$cmp" == "-1" ]]; then
      say "updating existing ${BINARY_NAME} ${existing_ver} → ${target_version} (${existing})"
    else
      warn "installed ${BINARY_NAME} ${existing_ver} is newer than requested ${target_version}; continuing because ${RELEASE_TAG} was requested"
    fi
  else
    say "found existing ${BINARY_NAME} at ${existing}; version unknown, reinstalling"
  fi
}

restart_existing_service() {
  local dest="$1"
  [[ "$OS" == "linux" ]] || return 0
  command -v systemctl >/dev/null 2>&1 || return 0
  systemctl --user list-unit-files dit.service >/dev/null 2>&1 || return 0

  local exec_start="" active=""
  exec_start="$(systemctl --user show dit.service -p ExecStart --value 2>/dev/null || true)"
  if [[ "$exec_start" != *"$dest"* ]]; then
    return 0
  fi

  active="$(systemctl --user is-active dit.service 2>/dev/null || true)"
  if [[ "$active" == "active" ]]; then
    say "restarting dit.service so it picks up the updated binary"
    systemctl --user restart dit.service && ok "dit.service restarted" || warn "could not restart dit.service; restart dit manually"
  elif pgrep -u "$(id -u)" -f "(^|/)${BINARY_NAME}([[:space:]]|$)" >/dev/null 2>&1; then
    warn "dit appears to be running outside the managed service; quit/restart it to use ${dest}"
  fi
}

# --- Linux runtime dependencies --------------------------------------------
# The Linux binary is self-contained (pure-Rust input/clipboard/tray); the only
# external library it needs is libasound, which every desktop already ships.
linux_deps() {
  [[ "$OS" == "linux" && "$SKIP_DEPS" == false ]] || return 0
  # The static (musl) build links libasound in; nothing to install.
  [[ "${VARIANT:-}" == "static" ]] && { ok "static build — no runtime libraries needed"; return 0; }

  if command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -q libasound.so.2; then
    ok "runtime libraries present"; return 0
  fi
  # dit's one and only runtime dependency is the ALSA shared library
  # (libasound2). The `-static` build avoids even this — see the hints below.
  local pm="" cmd=""
  if   command -v apt-get >/dev/null 2>&1; then pm=apt;    cmd="sudo apt-get install -y libasound2"
  elif command -v dnf     >/dev/null 2>&1; then pm=dnf;    cmd="sudo dnf install -y alsa-lib"
  elif command -v pacman  >/dev/null 2>&1; then pm=pacman; cmd="sudo pacman -S --needed --noconfirm alsa-lib"
  elif command -v zypper  >/dev/null 2>&1; then pm=zypper; cmd="sudo zypper install -y libasound2"
  else
    warn "dit needs the ALSA runtime library (libasound2) — install your distro's package, or\n      re-run with --static for the dependency-free build:\n        ${BINARY_NAME} update --force   # if dit is already installed\n        curl -fsSL .../install.sh | bash -s -- --static"
    return 0
  fi

  warn "missing libasound2 — dit's only runtime dependency"
  if [[ "$ASSUME_YES" == true ]] || confirm "Install it now with $pm?" yes; then
    say "running: $cmd"
    eval "$cmd" || warn "install failed — run it manually:\n      $cmd\n      …or use the dependency-free static build: re-run install.sh with --static"
  else
    warn "skipped — install later with:\n      $cmd\n      …or skip it entirely with the static build: re-run install.sh with --static"
  fi
}

# --- Linux input permissions (evdev read + uinput write) --------------------
# dit reads the hotkey from /dev/input and types via /dev/uinput on both X11 and
# Wayland, so this is needed on all Linux sessions.
linux_input_setup() {
  [[ "$OS" == "linux" && "$SKIP_DEPS" == false ]] || return 0

  local needs_group=true needs_udev=true
  id -nG 2>/dev/null | tr ' ' '\n' | grep -qx input && needs_group=false
  [[ -e /etc/udev/rules.d/99-uinput.rules ]] && needs_udev=false
  $needs_group || $needs_udev || { ok "input permissions OK"; return 0; }

  warn "dit reads the hotkey from /dev/input and types via /dev/uinput."
  if [[ "$ASSUME_YES" == true ]] || confirm "Set up the input-group + uinput udev rule now (sudo)?" yes; then
    $needs_group && { say "adding you to the 'input' group"; sudo usermod -aG input "$USER" || warn "usermod failed"; }
    if $needs_udev; then
      say "installing the uinput udev rule"
      echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
        | sudo tee /etc/udev/rules.d/99-uinput.rules >/dev/null \
        && sudo udevadm control --reload && sudo udevadm trigger || warn "udev setup failed"
    fi
    warn "log out and back in for the 'input' group to take effect, then start dit"
  else
    warn "set it up later:\n  sudo usermod -aG input \$USER\n  echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"' | sudo tee /etc/udev/rules.d/99-uinput.rules\n  sudo udevadm control --reload && sudo udevadm trigger\n  (then log out and back in)"
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

# --check-only: report whether a newer release exists, then exit without
# touching anything on disk. Backs `dit update --check`.
report_update_status() {
  local dest="$1" target existing existing_ver cmp
  target="$(normalize_version "$RELEASE_TAG")"
  existing="$(find_existing_binary "$dest" || true)"
  if [[ -z "$existing" ]]; then
    say "dit is not installed; latest release is ${RELEASE_TAG}"
    return 0
  fi
  existing_ver="$(version_of "$existing" || true)"
  if [[ -z "$existing_ver" ]]; then
    say "found dit at ${existing} (version unknown); latest is ${RELEASE_TAG}"
    return 0
  fi
  cmp="$(compare_versions "$existing_ver" "$target")"
  if [[ "$cmp" == "0" ]]; then
    ok "dit ${existing_ver} is already the latest release — nothing to update"
  elif [[ "$cmp" == "-1" ]]; then
    say "update available: ${existing_ver} → ${target} (run without --check-only to install)"
  else
    say "installed dit ${existing_ver} is newer than the latest release ${target}"
  fi
}

main() {
  detect_platform
  resolve_tag

  local dest="${INSTALL_DIR}/${BINARY_NAME}${EXT}"

  if [[ "$CHECK_ONLY" == true ]]; then
    report_update_status "$dest"
    exit 0
  fi

  choose_asset
  local asset="$ASSET"
  local url="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${asset}"
  ALREADY_CURRENT=false

  maybe_report_existing "$dest"

  if [[ "$ALREADY_CURRENT" == false ]]; then
    local tmp; tmp="$(mktemp)"
    trap 'rm -f "$tmp"' EXIT

    say "installing ${BINARY_NAME} ${RELEASE_TAG} (${PLATFORM}, ${VARIANT})"
    dl_to "$url" "$tmp" || die "Download failed: $url
This platform may not have a prebuilt binary — build from source instead (see the README)."
    verify_checksum "$tmp" "$asset"

    mkdir -p "$INSTALL_DIR"
    mv "$tmp" "$dest"; trap - EXIT
    if [[ "$OS" != "windows" ]]; then chmod +x "$dest"; fi
    # macOS: clear the Gatekeeper quarantine flag so it runs without a prompt.
    if [[ "$OS" == "macos" ]]; then xattr -d com.apple.quarantine "$dest" 2>/dev/null || true; fi
    ok "installed → ${dest}"
    restart_existing_service "$dest"
  else
    mkdir -p "$INSTALL_DIR"
  fi

  # PATH hint
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) warn "${INSTALL_DIR} is not on your PATH. Add it, e.g.:\n      echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile && source ~/.profile" ;;
  esac

  linux_deps
  linux_input_setup
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

#!/usr/bin/env bash
# Build Docker Stack from this checkout and install it (Linux and macOS).
#
#   scripts/install.sh [--deps] [--method METHOD] [--prefix DIR]
#   scripts/install.sh --check
#   scripts/install.sh --uninstall [--prefix DIR]
#
# Methods (Linux):
#   auto, user  build the binary and install it under --prefix (default ~/.local)
#               with a .desktop entry and icon; no sudo needed
# Methods (macOS):
#   auto, app  build "Docker Stack.app" and copy it into --prefix
#              (default /Applications; sudo only if that is not writable)
#
# --deps installs the build dependencies first: apt or dnf packages on Linux
# (sudo), the Xcode Command Line Tools on macOS.
# --check only reports build requirements and the Docker runtime, then exits
# (status 1 when a build requirement is missing). Nothing is built or installed.
# Also required: Rust (cargo). cargo fetches the noa-gpui UI toolkit from crates.io.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP=docker-stack
METHOD=auto
PREFIX=""
INSTALL_DEPS=0
UNINSTALL=0
CHECK_ONLY=0

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
usage() { sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --deps) INSTALL_DEPS=1 ;;
    --method) [ $# -ge 2 ] || die "--method needs a value"; METHOD="$2"; shift ;;
    --method=*) METHOD="${1#*=}" ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; PREFIX="$2"; shift ;;
    --prefix=*) PREFIX="${1#*=}" ;;
    --uninstall) UNINSTALL=1 ;;
    --check) CHECK_ONLY=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

OS="$(uname -s)"
case "$OS" in
  Linux)
    PREFIX="${PREFIX:-$HOME/.local}"
    case "$METHOD" in
      auto|user) METHOD=user ;;
      *) die "unknown method on Linux: $METHOD (auto|user)" ;;
    esac ;;
  Darwin)
    PREFIX="${PREFIX:-/Applications}"
    case "$METHOD" in
      auto|app) METHOD=app ;;
      *) die "unknown method on macOS: $METHOD (auto|app)" ;;
    esac ;;
  *) die "unsupported OS: $OS (Linux and macOS only)" ;;
esac

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO=sudo; fi
rpm_pm() { if have dnf; then echo dnf; else echo yum; fi; }

USER_BIN="$PREFIX/bin/$APP"
USER_DESKTOP="$PREFIX/share/applications/$APP.desktop"
USER_ICON="$PREFIX/share/icons/hicolor/128x128/apps/$APP.png"
USER_SVG="$PREFIX/share/icons/hicolor/scalable/apps/$APP.svg"
MAC_BUNDLE="Docker Stack.app"
MAC_APP="$PREFIX/$MAC_BUNDLE"

# sudo only when DIR (or its nearest existing parent) is not writable.
sudo_for() {
  local d="$1"
  while [ ! -e "$d" ]; do d="$(dirname "$d")"; done
  if [ -w "$d" ]; then echo ""; else echo "$SUDO"; fi
}

uninstall() {
  local removed=0
  if [ "$OS" = Darwin ]; then
    if [ -e "$MAC_APP" ]; then
      $(sudo_for "$PREFIX") rm -rf "$MAC_APP"; log "removed $MAC_APP"
    else
      log "nothing to uninstall at $MAC_APP"
    fi
    return
  fi
  for f in "$USER_BIN" "$USER_DESKTOP" "$USER_ICON" "$USER_SVG"; do
    if [ -e "$f" ]; then rm -f "$f"; log "removed $f"; removed=1; fi
  done
  [ "$removed" -eq 1 ] || log "nothing to uninstall"
}

if [ "$UNINSTALL" -eq 1 ]; then uninstall; exit 0; fi

install_deps() {
  if [ "$OS" = Darwin ]; then
    if xcode-select -p >/dev/null 2>&1; then
      log "Xcode Command Line Tools are already installed"
    else
      xcode-select --install || true
      die "finish the Xcode Command Line Tools installer, then re-run this script"
    fi
  elif have apt-get; then
    log "installing build dependencies with apt"
    $SUDO apt-get update
    $SUDO apt-get install -y build-essential pkg-config \
      libxkbcommon0 libvulkan1 mesa-vulkan-drivers
  elif have dnf || have yum; then
    log "installing build dependencies with $(rpm_pm)"
    $SUDO "$(rpm_pm)" install -y gcc gcc-c++ make pkgconf-pkg-config \
      libxkbcommon vulkan-loader mesa-vulkan-drivers
  else
    die "--deps supports apt and dnf/yum only; install a C toolchain, pkg-config and Vulkan drivers manually"
  fi
}

row() { printf '  %-8s %s\n' "$1" "$2"; }

xcode_clt() { xcode-select -p >/dev/null 2>&1; }
cc_present() { have cc || have gcc || have clang; }

# Prints each build requirement; sets MISSING to the ones that are absent.
# need LABEL HINT COMMAND...
build_report() {
  MISSING=()
  need() {
    local label="$1" hint="$2"
    shift 2
    if "$@"; then row ok "$label"; else row missing "$label${hint:+ ($hint)}"; MISSING+=("$label"); fi
  }
  echo "Build requirements"
  need "cargo" "https://rustup.rs" have cargo
  if [ "$OS" = Darwin ]; then
    need "Xcode Command Line Tools" "re-run with --deps" xcode_clt
  else
    need "C compiler" "re-run with --deps" cc_present
  fi
}

# Prints what the app will find at runtime. Informational: never fails the build.
runtime_report() {
  echo "Docker runtime"
  if have docker; then
    row ok "docker CLI ($(docker --version 2>/dev/null | sed 's/^Docker version //'))"
    if docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
      row ok "daemon reachable ($(docker context show 2>/dev/null || echo default))"
    else
      row note "daemon not reachable now; the app starts Docker Desktop or the docker service when it can"
    fi
    if docker compose version >/dev/null 2>&1; then
      row ok "docker compose v2"
    else
      row note "docker compose v2 not found; compose actions will be disabled"
    fi
  else
    row note "docker CLI not found; install Docker Engine (Linux) or Docker Desktop (macOS)"
  fi
  if [ "$OS" = Linux ]; then
    local sock=/var/run/docker.sock
    if [ -S "$sock" ] && ! [ -w "$sock" ]; then
      row note "$sock is not writable for $USER; run: sudo usermod -aG docker \$USER, then log in again"
    fi
    if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -S "$XDG_RUNTIME_DIR/docker.sock" ]; then
      row ok "rootless socket at $XDG_RUNTIME_DIR/docker.sock"
    fi
  fi
}

if [ "$CHECK_ONLY" -eq 1 ]; then
  build_report
  runtime_report
  [ "${#MISSING[@]}" -eq 0 ] || exit 1
  exit 0
fi

[ "$INSTALL_DEPS" -eq 1 ] && install_deps
build_report
runtime_report
[ "${#MISSING[@]}" -eq 0 ] || die "missing build requirements: ${MISSING[*]}"

cd "$ROOT"

case "$METHOD" in
  app)
    # build-app.sh builds the binary and assembles the bundle; this only copies it.
    "$ROOT/scripts/build-app.sh" --out "$ROOT/target/release"
    built="target/release/$MAC_BUNDLE"
    s="$(sudo_for "$PREFIX")"
    $s mkdir -p "$PREFIX"
    $s rm -rf "$MAC_APP"
    $s ditto "$built" "$MAC_APP"
    log "installed $MAC_APP"
    ;;
  user)
    log "building the binary (release)"
    cargo build --release -p docker-stack
    install -Dm755 "target/release/$APP" "$USER_BIN"
    install -Dm644 "app/icons/128x128.png" "$USER_ICON"
    install -Dm644 "app/icons/icon.svg" "$USER_SVG"
    mkdir -p "$(dirname "$USER_DESKTOP")"
    cat >"$USER_DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Docker Stack
Comment=Manage Docker containers, images, volumes and networks
Exec=$USER_BIN
Icon=$APP
Terminal=false
Categories=Development;
EOF
    have update-desktop-database && update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    log "installed $USER_BIN"
    case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) log "note: $PREFIX/bin is not on PATH" ;; esac
    ;;
esac

if [ "$OS" = Darwin ]; then
  log "done. Launch \"Docker Stack\" from Launchpad or run: open \"$MAC_APP\""
else
  log "done. Launch \"Docker Stack\" from the app menu or run: $APP"
fi

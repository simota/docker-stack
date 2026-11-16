#!/usr/bin/env bash
# Build "Docker Stack.app" from this checkout without installing it (macOS only).
#
#   scripts/build-app.sh [--out DIR]
#
# Builds the release binary and assembles the bundle at DIR/Docker Stack.app
# (default target/release). An existing bundle at that path is replaced.
# To build and copy it into /Applications in one step, use scripts/install.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP=docker-stack
BUNDLE="Docker Stack.app"
OUT="$ROOT/target/release"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
usage() { sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --out) [ $# -ge 2 ] || die "--out needs a value"; OUT="$2"; shift ;;
    --out=*) OUT="${1#*=}" ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

[ "$(uname -s)" = Darwin ] || die "a .app bundle can only be built on macOS"
command -v cargo >/dev/null 2>&1 || die "cargo not found (https://rustup.rs)"

cd "$ROOT"
log "building the binary (release)"
cargo build --release -p "$APP"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
dest="$OUT/$BUNDLE"
log "assembling $dest"
rm -rf "$dest"
mkdir -p "$dest/Contents/MacOS" "$dest/Contents/Resources"
cp "target/release/$APP" "$dest/Contents/MacOS/$APP"
cp "app/icons/icon.icns" "$dest/Contents/Resources/icon.icns"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$version" ] || die "could not read the version from Cargo.toml"
cat >"$dest/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Docker Stack</string>
  <key>CFBundleDisplayName</key><string>Docker Stack</string>
  <key>CFBundleIdentifier</key><string>dev.simota.docker-stack</string>
  <key>CFBundleExecutable</key><string>$APP</string>
  <key>CFBundleIconFile</key><string>icon.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$dest/Contents/Info.plist" >/dev/null || die "Info.plist is invalid"

log "built $dest (version $version)"
log "launch it with: open \"$dest\""

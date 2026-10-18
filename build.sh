#!/usr/bin/env bash
# Build term-chat, and on macOS wrap the binary in a `.app` bundle.
#
# Why: macOS ScreenCaptureKit can only capture audio at the APPLICATION level.
# A bare CLI run inside a terminal emulator lands in its own process group, so
# its TTS audio is invisible to OBS's "Application Audio Capture" (only a full
# desktop capture hears it). Running the binary from INSIDE a bundle
# (`term-chat.app/Contents/MacOS/term_display_rs`) makes macOS attribute the
# process — and its audio — to the app, so "term-chat" appears as a selectable
# application in OBS and its TTS is captured per-app.
#
# The manifest points `binary.macos.*` at the in-bundle path, so the supervisor
# launches it from inside the bundle automatically.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release "$@"

if [ "$(uname -s)" = "Darwin" ]; then
  BIN_NAME="term_display_rs"
  BIN="target/release/$BIN_NAME"
  APP="target/release/term-chat.app"
  if [ ! -x "$BIN" ]; then
    echo "build.sh: expected binary not found: $BIN" >&2
    exit 1
  fi
  rm -rf "$APP"
  mkdir -p "$APP/Contents/MacOS"
  cp "$BIN" "$APP/Contents/MacOS/$BIN_NAME"
  cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>term-chat</string>
  <key>CFBundleDisplayName</key><string>term-chat</string>
  <key>CFBundleIdentifier</key><string>com.vulbyte.cockatiel.term-chat</string>
  <key>CFBundleExecutable</key><string>$BIN_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST
  # Ad-hoc sign so the bundle has a stable identity (helps macOS attribute the
  # process/audio to it and avoids Gatekeeper friction for a local build).
  /usr/bin/codesign --force --deep --sign - "$APP" 2>/dev/null \
    || echo "build.sh: codesign skipped (not fatal)"
  echo "build.sh: bundled $APP"
fi

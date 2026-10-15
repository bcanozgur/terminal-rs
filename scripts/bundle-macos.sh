#!/usr/bin/env bash
# Wrap the release binary in a minimal macOS .app bundle (retina-capable, dark-mode aware).
#
#   scripts/bundle-macos.sh                 # target/release/terminal-rs
#   scripts/bundle-macos.sh path/to/binary  # another build of the terminal (e.g. with extra studies)
#
# The icon is assets/icon/terminal-rs.icns, made from terminal-rs-1024.png (each iconset size
# with `sips -z`, then `iconutil -c icns`).
set -euo pipefail
bin=${1:-}
[ -n "$bin" ] && bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
cd "$(dirname "$0")/.."
bin=${bin:-target/release/terminal-rs}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=target/release/terminal-rs.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/terminal-rs"
cp assets/icon/terminal-rs.icns "$app/Contents/Resources/terminal-rs.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>terminal-rs</string>
  <key>CFBundleDisplayName</key><string>terminal-rs</string>
  <key>CFBundleIdentifier</key><string>dev.terminal-rs.app</string>
  <key>CFBundleExecutable</key><string>terminal-rs</string>
  <key>CFBundleIconFile</key><string>terminal-rs</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.finance</string>
</dict>
</plist>
PLIST
echo "bundled $app"

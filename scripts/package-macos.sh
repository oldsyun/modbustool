#!/usr/bin/env bash
# Build a macOS .app bundle + .dmg installer for Modbus Tool (GPUI).
# Usage: ./scripts/package-macos.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

APP_NAME="Modbus Tool"
BUNDLE_ID="com.modbus.tool"
VERSION="0.1.0"
BIN="target-gpui/release/modbus-tool"
DIST="dist"
APP="$DIST/$APP_NAME.app"
PLIST_DIR="$APP/Contents"

echo "==> Building release binary"
./scripts/cargo-gpui.sh build --release

echo "==> Preparing .app bundle"
rm -rf "$APP"
mkdir -p "$PLIST_DIR/MacOS" "$PLIST_DIR/Resources"

cp "$BIN" "$PLIST_DIR/MacOS/modbus-tool"
chmod +x "$PLIST_DIR/MacOS/modbus-tool"

# ── Icon: build .icns from the 1024 PNG ──
ICON_SRC="app/resources/icon_1024.png"
if [ -f "$ICON_SRC" ]; then
  ICONSET="$DIST/AppIcon.iconset"
  rm -rf "$ICONSET"; mkdir -p "$ICONSET"
  for sz in 16 32 64 128 256 512; do
    sips -z $sz $sz "$ICON_SRC" --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
    d=$((sz*2))
    sips -z $d $d "$ICON_SRC" --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$PLIST_DIR/Resources/AppIcon.icns"
  rm -rf "$ICONSET"
fi

cat > "$PLIST_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>modbus-tool</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# Ad-hoc sign so the bundle runs locally without Gatekeeper complaints.
echo "==> Ad-hoc code signing"
codesign --force --deep --sign - "$APP" 2>&1 | tail -2 || echo "(codesign skipped)"

echo "==> Zipping .app (portable installer)"
ZIP="$DIST/ModbusTool-${VERSION}-macos.zip"
rm -f "$ZIP"
( cd "$DIST" && zip -qry "$(basename "$ZIP")" "$APP_NAME.app" )

echo "==> Creating DMG (best-effort; may be unavailable in restricted sandboxes)"
DMG="$DIST/ModbusTool-${VERSION}-macos.dmg"
rm -f "$DMG"
STAGING="$DIST/dmg-staging"
rm -rf "$STAGING"; mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
if hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG" >/dev/null 2>&1; then
  echo "    DMG created: $DMG"
else
  echo "    (DMG skipped — use the .zip; zip contains the same .app)"
  rm -f "$DMG"
fi
rm -rf "$STAGING"

echo ""
echo "==> Done"
echo "  App:  $APP"
echo "  Zip:  $ZIP"
[ -f "$DMG" ] && echo "  DMG:  $DMG"
ls -lh "$ZIP" "$PLIST_DIR/MacOS/modbus-tool"

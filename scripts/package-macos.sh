#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.1.0}"
mkdir -p dist "downloadcraft.app/Contents/MacOS" "downloadcraft.app/Contents/Resources"
# universal binary built by workflow via lipo into target/universal; fallback to release dir
if [ -f target/universal/downloadcraft ]; then SRC=target/universal; else SRC=target/release; fi
cp "$SRC/downloadcraft" "downloadcraft.app/Contents/MacOS/downloadcraft"
cp "$SRC/downloadcraft-host" "downloadcraft.app/Contents/MacOS/downloadcraft-host" || true
cp packaging/Info.plist "downloadcraft.app/Contents/Info.plist"
cp assets/downloadcraft.icns "downloadcraft.app/Contents/Resources/downloadcraft.icns"
chmod +x "downloadcraft.app/Contents/MacOS/"*
if command -v hdiutil >/dev/null 2>&1; then
  hdiutil create -volname downloadcraft -srcfolder downloadcraft.app -ov -format UDZO "dist/downloadcraft-${VERSION}-universal.dmg" || ditto -c -k --keepParent downloadcraft.app "dist/downloadcraft-${VERSION}-universal.zip"
else
  ditto -c -k --keepParent downloadcraft.app "dist/downloadcraft-${VERSION}-universal.zip" || zip -r "dist/downloadcraft-${VERSION}-universal.zip" downloadcraft.app
fi
ls -la dist/

#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.2.0}"
mkdir -p dist "webcraft.app/Contents/MacOS" "webcraft.app/Contents/Resources"
# universal binary built by workflow via lipo into target/universal; fallback to release dir
if [ -f target/universal/webcraft ]; then SRC=target/universal; else SRC=target/release; fi
cp "$SRC/webcraft" "webcraft.app/Contents/MacOS/webcraft"
cp "$SRC/webcraft-host" "webcraft.app/Contents/MacOS/webcraft-host" || true
cp packaging/Info.plist "webcraft.app/Contents/Info.plist"
cp assets/webcraft.icns "webcraft.app/Contents/Resources/webcraft.icns"
chmod +x "webcraft.app/Contents/MacOS/"*
if command -v hdiutil >/dev/null 2>&1; then
  hdiutil create -volname webcraft -srcfolder webcraft.app -ov -format UDZO "dist/webcraft-${VERSION}-universal.dmg" || ditto -c -k --keepParent webcraft.app "dist/webcraft-${VERSION}-universal.zip"
else
  ditto -c -k --keepParent webcraft.app "dist/webcraft-${VERSION}-universal.zip" || zip -r "dist/webcraft-${VERSION}-universal.zip" webcraft.app
fi
ls -la dist/

#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.1.0}"
mkdir -p dist "idmcraft.app/Contents/MacOS" "idmcraft.app/Contents/Resources"
# universal binary built by workflow via lipo into target/universal; fallback to release dir
if [ -f target/universal/idmcraft ]; then SRC=target/universal; else SRC=target/release; fi
cp "$SRC/idmcraft" "idmcraft.app/Contents/MacOS/idmcraft"
cp "$SRC/idmcraft-host" "idmcraft.app/Contents/MacOS/idmcraft-host" || true
cp packaging/Info.plist "idmcraft.app/Contents/Info.plist"
chmod +x "idmcraft.app/Contents/MacOS/"*
if command -v hdiutil >/dev/null 2>&1; then
  hdiutil create -volname idmcraft -srcfolder idmcraft.app -ov -format UDZO "dist/idmcraft-${VERSION}-universal.dmg" || ditto -c -k --keepParent idmcraft.app "dist/idmcraft-${VERSION}-universal.zip"
else
  ditto -c -k --keepParent idmcraft.app "dist/idmcraft-${VERSION}-universal.zip" || zip -r "dist/idmcraft-${VERSION}-universal.zip" idmcraft.app
fi
ls -la dist/

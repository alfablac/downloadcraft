#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.2.0}"
ROOT="$(pwd)"
mkdir -p dist staging/usr/bin staging/usr/share/applications staging/usr/share/icons/hicolor/512x512/apps staging/DEBIAN
cp target/release/webcraft staging/usr/bin/
cp target/release/webcraft-host staging/usr/bin/
cp packaging/webcraft.desktop staging/usr/share/applications/
cp assets/webcraft.png staging/usr/share/icons/hicolor/512x512/apps/webcraft.png
cat > staging/DEBIAN/control <<EOF
Package: webcraft
Version: ${VERSION}
Section: net
Priority: optional
Architecture: amd64
Maintainer: webcraft contributors
Description: Cross-platform download manager (clean-room IDM clone)
EOF
dpkg-deb --build staging "dist/webcraft_${VERSION}_amd64.deb" || echo "dpkg-deb unavailable, skipping .deb"
# AppImage-style directory + tarball fallback (AppImage needs linuxdeploy/appimagetool; tarball always works)
mkdir -p AppDir/usr/bin
cp target/release/webcraft target/release/webcraft-host AppDir/usr/bin/
cp packaging/webcraft.desktop AppDir/
cp assets/webcraft.png AppDir/webcraft.png
cat > AppDir/AppRun <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/usr/bin/webcraft" "$@"
EOF
chmod +x AppDir/AppRun AppDir/usr/bin/*
if command -v appimagetool >/dev/null 2>&1; then
  appimagetool AppDir "dist/webcraft-${VERSION}-x86_64.AppImage" || true
fi
tar -czf "dist/webcraft-${VERSION}-linux-x86_64.tar.gz" -C AppDir .
ls -la dist/

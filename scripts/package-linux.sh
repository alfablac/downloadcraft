#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.1.0}"
ROOT="$(pwd)"
mkdir -p dist staging/usr/bin staging/usr/share/applications staging/usr/share/icons/hicolor/512x512/apps staging/DEBIAN
cp target/release/downloadcraft staging/usr/bin/
cp target/release/downloadcraft-host staging/usr/bin/
cp packaging/downloadcraft.desktop staging/usr/share/applications/
cp assets/downloadcraft.png staging/usr/share/icons/hicolor/512x512/apps/downloadcraft.png
cat > staging/DEBIAN/control <<EOF
Package: downloadcraft
Version: ${VERSION}
Section: net
Priority: optional
Architecture: amd64
Maintainer: downloadcraft contributors
Description: Cross-platform download manager (clean-room IDM clone)
EOF
dpkg-deb --build staging "dist/downloadcraft_${VERSION}_amd64.deb" || echo "dpkg-deb unavailable, skipping .deb"
# AppImage-style directory + tarball fallback (AppImage needs linuxdeploy/appimagetool; tarball always works)
mkdir -p AppDir/usr/bin
cp target/release/downloadcraft target/release/downloadcraft-host AppDir/usr/bin/
cp packaging/downloadcraft.desktop AppDir/
cp assets/downloadcraft.png AppDir/downloadcraft.png
cat > AppDir/AppRun <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/usr/bin/downloadcraft" "$@"
EOF
chmod +x AppDir/AppRun AppDir/usr/bin/*
if command -v appimagetool >/dev/null 2>&1; then
  appimagetool AppDir "dist/downloadcraft-${VERSION}-x86_64.AppImage" || true
fi
tar -czf "dist/downloadcraft-${VERSION}-linux-x86_64.tar.gz" -C AppDir .
ls -la dist/

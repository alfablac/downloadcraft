#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-0.1.0}"
ROOT="$(pwd)"
mkdir -p dist staging/usr/bin staging/usr/share/applications staging/DEBIAN
cp target/release/idmcraft staging/usr/bin/
cp target/release/idmcraft-host staging/usr/bin/
cp packaging/idmcraft.desktop staging/usr/share/applications/
cat > staging/DEBIAN/control <<EOF
Package: idmcraft
Version: ${VERSION}
Section: net
Priority: optional
Architecture: amd64
Maintainer: idmcraft contributors
Description: Cross-platform download manager (clean-room IDM clone)
EOF
dpkg-deb --build staging "dist/idmcraft_${VERSION}_amd64.deb" || echo "dpkg-deb unavailable, skipping .deb"
# AppImage-style directory + tarball fallback (AppImage needs linuxdeploy/appimagetool; tarball always works)
mkdir -p AppDir/usr/bin
cp target/release/idmcraft target/release/idmcraft-host AppDir/usr/bin/
cp packaging/idmcraft.desktop AppDir/
cat > AppDir/AppRun <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/usr/bin/idmcraft" "$@"
EOF
chmod +x AppDir/AppRun AppDir/usr/bin/*
if command -v appimagetool >/dev/null 2>&1; then
  appimagetool AppDir "dist/idmcraft-${VERSION}-x86_64.AppImage" || true
fi
tar -czf "dist/idmcraft-${VERSION}-linux-x86_64.tar.gz" -C AppDir .
ls -la dist/

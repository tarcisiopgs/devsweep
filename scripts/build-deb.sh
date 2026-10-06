#!/bin/sh
# Wrap a devsweep binary already built for Linux in a .deb:
#
#   scripts/build-deb.sh <binary> <version> <amd64|arm64> <output-folder>
#
# The binary is static, so the package depends on nothing. Needs dpkg-deb,
# which the Linux runners of the release have.
set -eu

bin=$1
version=$2
arch=$3
out=$4

root=$(cd "$(dirname "$0")/.." && pwd)
pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
# mktemp makes the folder private; a package's folders are world-readable.
chmod 755 "$pkg"

install -D -m 755 "$bin" "$pkg/usr/bin/devsweep"
install -D -m 644 "$root/LICENSE" "$pkg/usr/share/doc/devsweep/copyright"
size=$(du -sk "$pkg/usr" | cut -f 1)

mkdir "$pkg/DEBIAN"
cat > "$pkg/DEBIAN/control" <<EOF
Package: devsweep
Version: $version
Architecture: $arch
Maintainer: Tarcísio Pedro <tarcisiopgs@gmail.com>
Installed-Size: $size
Section: utils
Priority: optional
Homepage: https://github.com/tarcisiopgs/devsweep
Description: Find and remove what a developer's machine accumulates
 Build artifacts, agent worktrees, emulators, Docker leftovers and dev tool
 caches, picked in a terminal UI and removed only after a review.
EOF

mkdir -p "$out"
dpkg-deb --root-owner-group --build "$pkg" "$out/devsweep_${version}_${arch}.deb"

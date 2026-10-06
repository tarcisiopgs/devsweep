#!/bin/sh
# Point the winget manifest kept in this repository at a release:
#
#   scripts/update-packaging.sh v0.4.1
#
# It rewrites packaging/winget from the checksums the release publishes.
# Run it once the release has its binaries; publishing the result is
# described in AGENTS.md.
set -eu

tag=$1
version=${tag#v}
repo=tarcisiopgs/devsweep
url=https://github.com/$repo
base=$url/releases/download/$tag
root=$(cd "$(dirname "$0")/.." && pwd)

sum() {
    curl -fsSL "$base/$1.sha256" | cut -d " " -f 1
}
# winget writes hashes in upper case.
win_x64=$(sum devsweep-x86_64-pc-windows-msvc.zip | tr a-f A-F)
win_arm=$(sum devsweep-aarch64-pc-windows-msvc.zip | tr a-f A-F)
released=$(gh release view "$tag" --repo "$repo" --json publishedAt --jq '.publishedAt[0:10]')

winget=$root/packaging/winget
mkdir -p "$winget"
cat > "$winget/tarcisiopgs.devsweep.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json

PackageIdentifier: tarcisiopgs.devsweep
PackageVersion: $version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.12.0
EOF
cat > "$winget/tarcisiopgs.devsweep.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json

PackageIdentifier: tarcisiopgs.devsweep
PackageVersion: $version
InstallerType: zip
NestedInstallerType: portable
Commands:
- devsweep
ReleaseDate: $released
Installers:
- Architecture: x64
  InstallerUrl: $base/devsweep-x86_64-pc-windows-msvc.zip
  InstallerSha256: $win_x64
  NestedInstallerFiles:
  - RelativeFilePath: devsweep-x86_64-pc-windows-msvc\\devsweep.exe
    PortableCommandAlias: devsweep
- Architecture: arm64
  InstallerUrl: $base/devsweep-aarch64-pc-windows-msvc.zip
  InstallerSha256: $win_arm
  NestedInstallerFiles:
  - RelativeFilePath: devsweep-aarch64-pc-windows-msvc\\devsweep.exe
    PortableCommandAlias: devsweep
ManifestType: installer
ManifestVersion: 1.12.0
EOF
cat > "$winget/tarcisiopgs.devsweep.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json

PackageIdentifier: tarcisiopgs.devsweep
PackageVersion: $version
PackageLocale: en-US
Publisher: Tarcísio Pedro
PublisherUrl: https://github.com/tarcisiopgs
PublisherSupportUrl: $url/issues
PackageName: devsweep
PackageUrl: $url
License: MIT
LicenseUrl: $url/blob/$tag/LICENSE
ShortDescription: Find and remove what a developer's machine accumulates.
Description: A terminal UI that finds build artifacts, git worktrees left by coding agents, emulators, Docker leftovers and dev tool caches, and removes what you pick after a review.
Tags:
- cleanup
- cli
- developer-tools
- disk
- rust
- tui
ReleaseNotesUrl: $url/releases/tag/$tag
ManifestType: defaultLocale
ManifestVersion: 1.12.0
EOF
echo "packaging/ now points at $tag"

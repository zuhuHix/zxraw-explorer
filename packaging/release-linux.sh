#!/usr/bin/env bash
# Builds the Linux release files for a version that is already set in the tree, then prints the
# command that adds them to the existing GitHub release.
#
# Usage: packaging/release-linux.sh <x.y.z> [--skip-build]
#
# 1. Checks that [workspace.package] version in Cargo.toml equals <x.y.z>. This script does NOT
#    bump the version: packaging/release.ps1 does that on Windows, and the Linux build runs on the
#    same bumped tree.
# 2. Runs packaging/linux/build-linux.sh (AppImage, .deb, SHA256SUMS-linux.txt in dist/).
# 3. Prints the `gh release upload` command. It never runs gh, never commits, tags, pushes or
#    uploads anything itself.
#
# Full steps: packaging/linux/README.md.
set -euo pipefail

die() { echo "release-linux.sh: $*" >&2; exit 1; }

if (($# < 1)) || [[ "$1" == -h || "$1" == --help ]]; then
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit $(( $# < 1 ? 2 : 0 ))
fi
VERSION="$1"
shift
build_args=()
for arg in "$@"; do
    case "$arg" in
        --skip-build) build_args+=("$arg") ;;
        *) die "unknown argument: $arg" ;;
    esac
done
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must look like 1.2.3 (no 'v' prefix): $VERSION"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
repo_slug="zuhuHix/zxraw-explorer"
cd "$repo"

tree_version="$(python3 -I -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["version"])
')"
if [[ "$tree_version" != "$VERSION" ]]; then
    die "Cargo.toml has version $tree_version, not $VERSION. Bump the version (release.ps1 does this on Windows) and commit it before the Linux build."
fi

"$here/linux/build-linux.sh" "${build_args[@]}"

appimage="dist/ZXraw-Explorer-$VERSION-x86_64.AppImage"
deb="dist/zxraw_${VERSION}_amd64.deb"
sums="dist/SHA256SUMS-linux.txt"
for f in "$appimage" "$deb" "$sums"; do
    [[ -f "$f" ]] || die "expected output missing: $f"
done

cat <<EOF

Linux files are ready in $repo/dist:
  $appimage
  $deb
  $sums

Nothing has been uploaded. To add them to the existing v$VERSION release (after the Windows
release exists, and after the version commit is tagged and pushed), run:

  gh release upload v$VERSION $appimage $deb $sums --clobber --repo $repo_slug

Check the SHA256SUMS-linux.txt values against the files before publishing.
EOF

#!/usr/bin/env bash
# Sets the release version in the tree: the Linux/bash counterpart of step 1 of
# packaging/release.ps1.
#
# Usage: packaging/bump-version.sh <x.y.z> [--dry-run]
#
#   - [workspace.package] version in Cargo.toml
#   - #define AppVersion in packaging/zxraw.iss
#   - the workspace entries in Cargo.lock (cargo update --workspace)
#
# Running it again with the same version changes nothing. --dry-run prints what would change and
# writes nothing. It never commits, tags or pushes; see packaging/RELEASING.md for the next steps.
set -euo pipefail

die() { echo "bump-version.sh: $*" >&2; exit 1; }

usage() { sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

VERSION=""
DRY_RUN=0
while (($#)); do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        -h|--help) usage; exit 0 ;;
        -*) usage >&2; die "unknown option: $1" ;;
        *) [[ -z "$VERSION" ]] || die "more than one version given"; VERSION="$1" ;;
    esac
    shift
done
[[ -n "$VERSION" ]] || { usage >&2; exit 2; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "version must look like 1.2.3 (no 'v' prefix, no pre-release suffix): $VERSION"

for tool in cargo python3; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
cd "$repo"

# Edits both files in place with the same patterns release.ps1 uses, keeping line endings and
# encoding (bytes in, bytes out). Prints "<file>: <old> -> <new>" or "<file>: already <new>".
python3 -I - "$VERSION" "$DRY_RUN" <<'PY'
import re, sys

version, dry_run = sys.argv[1], sys.argv[2] == "1"
edits = [
    ("Cargo.toml",
     rb'(?ms)(^\[workspace\.package\][^\[]*?^version\s*=\s*")([^"]+)(")'),
    ("packaging/zxraw.iss",
     rb'(?m)^(#define AppVersion ")([^"]+)(")'),
]
for path, pattern in edits:
    with open(path, "rb") as f:
        data = f.read()
    m = re.search(pattern, data)
    if not m:
        sys.exit(f"bump-version.sh: could not find the version in {path}")
    old = m.group(2).decode()
    if old == version:
        print(f"{path}: already {version}")
        continue
    print(f"{path}: {old} -> {version}" + (" (dry run, not written)" if dry_run else ""))
    if not dry_run:
        new = data[:m.start(2)] + version.encode() + data[m.end(2):]
        with open(path, "wb") as f:
            f.write(new)
PY

if (( DRY_RUN )); then
    echo "Cargo.lock: would run cargo update --workspace (dry run, not run)"
    exit 0
fi

echo "Updating Cargo.lock for version $VERSION..."
if ! cargo update --workspace --offline; then
    cargo update --workspace || die "cargo update --workspace failed"
fi

echo
git -C "$repo" status --short -- Cargo.toml Cargo.lock packaging/zxraw.iss
cat <<EOF

Next (see packaging/RELEASING.md):
  git commit -am "Release $VERSION"
  git tag v$VERSION
  git push origin HEAD v$VERSION     # the Release workflow builds a draft release
EOF

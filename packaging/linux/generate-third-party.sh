#!/usr/bin/env bash
# Writes packaging/linux/THIRD_PARTY-linux.md: the Linux-target counterpart of
# packaging/THIRD_PARTY.md (which packaging/generate-third-party.ps1 writes for Windows).
#
# Same scope and format as the Windows file: forge-app and forge-cli plus their resolved
# normal runtime dependencies for x86_64-unknown-linux-gnu (dev and build dependencies are
# excluded), the same license allowlist and Review column, and the UnRAR licence section,
# copied verbatim from packaging/THIRD_PARTY.md. Needs cargo and python3 (no jq, no pwsh).
#
# Usage: packaging/linux/generate-third-party.sh [output-path]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
output="${1:-$here/THIRD_PARTY-linux.md}"
windows_inventory="$repo/packaging/THIRD_PARTY.md"

[[ -f "$windows_inventory" ]] || { echo "missing $windows_inventory" >&2; exit 1; }

cd "$repo"
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu \
    | python3 -I -c '
import json, os, re, sys

output, windows_inventory = sys.argv[1], sys.argv[2]
metadata = json.load(sys.stdin)

packages = {p["id"]: p for p in metadata["packages"]}
nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
roots = [p for p in metadata["packages"] if p["name"] in ("forge-app", "forge-cli")]
names = {p["name"] for p in roots}
if "forge-app" not in names or "forge-cli" not in names:
    sys.exit("cargo metadata did not include both forge-app and forge-cli")

# Resolved normal (non-dev, non-build) dependencies, transitively, from the two roots.
included = set()
pending = [p["id"] for p in roots]
while pending:
    package_id = pending.pop()
    if package_id in included:
        continue
    included.add(package_id)
    node = nodes.get(package_id)
    if node is None:
        continue
    for dep in node["deps"]:
        kinds = dep.get("dep_kinds") or []
        if any(k.get("kind") is None for k in kinds):
            pending.append(dep["pkg"])

PERMISSIVE = {
    "0BSD", "Apache-2.0", "BSD-0-Clause", "BSD-2-Clause", "BSD-3-Clause", "BSD-4-Clause",
    "BSL-1.0", "CC0-1.0", "ISC", "MIT", "MIT-0", "Unicode-3.0", "Unicode-DFS-2016",
    "Unlicense", "Zlib",
}

def escape(value):
    return value.replace("|", "\\|").replace("\r", " ").replace("\n", " ")

rows = []
for package_id in included:
    package = packages[package_id]
    expression = package.get("license") or ""
    base = re.sub(r"\s+WITH\s+[A-Za-z0-9.+-]+", " ", expression)
    tokens = [t for t in re.findall(r"[A-Za-z0-9][A-Za-z0-9.+-]*", base)
              if t not in ("AND", "OR", "WITH")]
    non_permissive = [t for t in tokens if t not in PERMISSIVE]
    if tokens and not non_permissive:
        review = "Permissive"
    elif "MPL-2.0" in expression:
        review = "FLAG: MPL-2.0 weak copyleft review"
    else:
        review = "FLAG: license review"
    rows.append({
        "name": package["name"],
        "version": package["version"],
        "license": expression.strip() or "Not declared",
        "repository": (package.get("repository") or "").strip() or "Not declared",
        "review": review,
    })

rows.sort(key=lambda r: (r["name"].lower(), r["version"]))
flagged = [r for r in rows if r["review"].startswith("FLAG:")]

with open(windows_inventory, encoding="utf-8") as f:
    windows_text = f.read()
unrar = windows_text[windows_text.index("## UnRAR"):].rstrip("\n")

lines = [
    "# Third-party inventory",
    "",
    "Generated from cargo metadata for x86_64-unknown-linux-gnu.",
    "Scope: forge-app and forge-cli, plus their resolved normal runtime dependencies; dev and build dependencies are excluded.",
    "",
    "| Crate | Version | License expression | Repository | Review |",
    "| --- | --- | --- | --- | --- |",
]
for r in rows:
    lines.append("| {} | {} | {} | {} | {} |".format(
        escape(r["name"]), escape(r["version"]), escape(r["license"]),
        escape(r["repository"]), escape(r["review"])))
lines += [
    "",
    "## License notes",
    "",
    "The report flags missing or non-permissive license expressions for release review. The allowlist recognizes MIT, MIT-0, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0-1.0, BSL-1.0, and Unlicense expressions. SPDX WITH exceptions are reported with their base license.",
    "MPL-2.0 is weak copyleft with file-level source and notice obligations; any dependency declaring it is flagged for review.",
]
if flagged:
    lines.append("Flagged dependencies: " + "; ".join(
        "{} {} ({}; {})".format(r["name"], r["version"], r["review"], r["license"]) for r in flagged))
else:
    lines.append("No dependencies were flagged by the license allowlist.")
lines += [
    "FFmpeg and ffprobe are optional external programs and are not bundled with ZXraw Explorer.",
    "",
    unrar,
]
with open(output, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(lines) + "\n")
print("Wrote {} crate entries to {}".format(len(rows), output))
print("License flags: {}".format(len(flagged)))
' "$output" "$windows_inventory"

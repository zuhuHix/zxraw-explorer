#!/usr/bin/env bash
# Builds the Linux release files into dist/:
#   dist/ZXraw-Explorer-<version>-x86_64.AppImage
#   dist/zxraw_<version>_amd64.deb
#   dist/SHA256SUMS-linux.txt
#
# The version is read from [workspace.package] in Cargo.toml. Nothing is installed system-wide:
# pinned releases of appimagetool and the AppImage runtime are cached under
# ${XDG_CACHE_HOME:-$HOME/.cache}/zxraw-packaging/ and checked against the SHA-256 values below
# before every use. Nothing is tagged, pushed or uploaded. See packaging/linux/README.md.
#
# Usage: packaging/linux/build-linux.sh [--skip-build]
#   --skip-build  package the existing release binary without running cargo
set -euo pipefail

die() { echo "build-linux.sh: $*" >&2; exit 1; }

usage() {
    sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

SKIP_BUILD=0
while (($#)); do
    case "$1" in
        --skip-build) SKIP_BUILD=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
    shift
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

for tool in cargo python3 ldd dpkg dpkg-deb dpkg-query readlink sha256sum objdump strip curl install; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# The outputs are labelled x86_64 (AppImage) and amd64 (.deb), and the pinned tools below are
# x86_64 builds.
host_arch="$(uname -m)"
[[ "$host_arch" == x86_64 ]] || die "this script builds x86_64/amd64 packages; the host is $host_arch"

# Pinned packaging tools: exact releases, verified by SHA-256 before they run. To update, change
# the URL and the hash together (GitHub shows each asset's sha256 digest on the release page).
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
# Passed to appimagetool with --runtime-file, so it does not download a runtime itself.
RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64"
RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"

VERSION="$(python3 -I -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["version"])
')"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "unexpected version in Cargo.toml: $VERSION"

target_dir="${CARGO_TARGET_DIR:-target}"
[[ "$target_dir" = /* ]] || target_dir="$repo/$target_dir"
export CARGO_TARGET_DIR="$target_dir"
binary="$target_dir/release/zxraw"
cli_binary="$target_dir/release/zxraw-cli"

if (( ! SKIP_BUILD )); then
    echo "Building zxraw and zxraw-cli $VERSION (release)..."
    CARGO_PROFILE_DEV_DEBUG=0 cargo build --release --no-default-features -p forge-app -p forge-cli
else
    echo "Skipping cargo build (--skip-build)"
fi
[[ -x "$binary" ]] || die "release binary missing: $binary (run without --skip-build)"
[[ -x "$cli_binary" ]] || die "release binary missing: $cli_binary (run without --skip-build)"

# Inputs that the packages ship.
icons_src="$here/icons/hicolor"
for n in 16 24 32 48 64 128 256; do
    [[ -f "$icons_src/${n}x${n}/apps/zxraw.png" ]] || die "missing icon ${n}x${n} (run make-icons.sh)"
done
desktop_src="$here/zxraw.desktop"
[[ -f "$desktop_src" ]] || die "missing $desktop_src"
copyright_src="$here/copyright"
[[ -f "$copyright_src" ]] || die "missing $copyright_src"
third_party_src="$here/THIRD_PARTY-linux.md"
[[ -f "$third_party_src" ]] || die "missing $third_party_src (run generate-third-party.sh)"
for f in "$repo/LICENSE-MIT" "$repo/LICENSE-APACHE" "$repo/packaging/LICENSES-NOTE.md" "$repo/packaging/LICENSE-UNRAR.txt"; do
    [[ -f "$f" ]] || die "missing license file: $f"
done

# Shared libraries. The binary's own dependencies come from ldd. The windowing, GPU and desktop
# libraries (winit and wgpu load several of them with dlopen) come from the host, so they are not
# bundled. Anything else that is linked is bundled into the AppImage. The .deb never bundles: each
# linked library becomes a Depends on its Debian package.
ldd_out="$(ldd "$binary" "$cli_binary")"
if grep -q "not found" <<<"$ldd_out"; then
    die "unresolved shared libraries:"$'\n'"$ldd_out"
fi

host_provided='^(linux-vdso|ld-linux|libc\.so|libm\.so|libdl\.so|libpthread\.so|librt\.so|libutil\.so|libresolv\.so|libgcc_s\.so|libstdc\+\+\.so|libGL|libEGL|libGLX|libGLdispatch|libvulkan|libX11|libXcursor|libXrandr|libXi|libXext|libxcb|libxkbcommon|libwayland|libdrm|libgbm|libasound|libpulse|libudev|libdbus|libfontconfig|libfreetype|libz\.so|libexpat|libgio|libglib|libgobject|libgmodule|libgtk|libsystemd|libselinux|libcap|libnsl)'

declare -a linked_libs=()
declare -A linked_path=()
declare -A pkg_of_soname=()
declare -a bundled_sonames=()
while read -r soname path; do
    [[ -n "$soname" ]] || continue
    real="$(readlink -f "$path")"
    pkg="$(dpkg -S "$real" 2>/dev/null | head -n1 | cut -d: -f1 || true)"
    [[ -n "$pkg" ]] || die "no Debian package owns $real (needed for the .deb Depends)"
    linked_libs+=("$soname")
    linked_path["$soname"]="$real"
    pkg_of_soname["$soname"]="$pkg"
    if [[ ! "$soname" =~ $host_provided ]]; then
        bundled_sonames+=("$soname")
    fi
done < <(awk '/=>/ && $3 ~ /^\// { print $1, $3 }' <<<"$ldd_out" | sort -u)

# Highest GLIBC symbol version the binaries need, for the libc6 dependency.
glibc_need="$(objdump -T "$binary" "$cli_binary" | grep -o 'GLIBC_[0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n1 || true)"
[[ -n "$glibc_need" ]] || glibc_need="2.31"

# Depends: the packages of the linked libraries. Recommends: runtime-loaded libraries, unless a
# Depends already covers them.
declare -a depends=()
mapfile -t depends < <(printf '%s\n' "${pkg_of_soname[@]}" | sort -u)
depends_line="libc6 (>= $glibc_need)"
for pkg in "${depends[@]}"; do
    [[ "$pkg" == libc6 ]] && continue
    depends_line+=", $pkg"
done
# Loaded with dlopen, so ldd does not list them, but winit cannot start without them: the
# keymap library on X11 and Wayland, and its X11 part (the app panics on X11 without it).
for pkg in libxkbcommon0 libxkbcommon-x11-0; do
    depends+=("$pkg")
    depends_line+=", $pkg"
done

recommends_candidates=("libvulkan1 | libgl1" "libwayland-client0" "libx11-6" "xdg-utils")
declare -a recommends=()
for item in "${recommends_candidates[@]}"; do
    covered=0
    IFS='|' read -ra alts <<<"$item"
    for alt in "${alts[@]}"; do
        alt="${alt// /}"
        for pkg in "${depends[@]}"; do
            [[ "$pkg" == "$alt" ]] && covered=1
        done
    done
    (( covered )) || recommends+=("$item")
done
recommends_line=""
for item in "${recommends[@]}"; do
    recommends_line+="${recommends_line:+, }$item"
done

echo "Linked shared libraries (soname -> package):"
for soname in "${linked_libs[@]}"; do
    printf '  %-28s %s\n' "$soname" "${pkg_of_soname[$soname]}"
done
echo "Bundled into the AppImage: ${bundled_sonames[*]:-(none)}"
echo "Depends: $depends_line"
echo "Recommends: ${recommends_line:-(none)}"

# Output and scratch space.
dist="$repo/dist"
mkdir -p "$dist"
work="$target_dir/packaging-linux"
rm -rf "$work"
mkdir -p "$work"

# The packaged copy has its DWARF debug info removed (the release profile keeps line tables,
# which makes the binary about 4x larger). The symbol table stays, so backtraces keep function
# names. The copy in target/release is left as built.
staged_binary="$work/zxraw"
install -m755 "$binary" "$staged_binary"
strip --strip-debug "$staged_binary"
# The command-line tool ships next to the app, as in the Windows zip.
staged_cli="$work/zxraw-cli"
install -m755 "$cli_binary" "$staged_cli"
strip --strip-debug "$staged_cli"

appimage_name="ZXraw-Explorer-$VERSION-x86_64.AppImage"
deb_name="zxraw_${VERSION}_amd64.deb"

install_docs() {
    # Usage: install_docs <doc dir> appimage|deb
    # The AppImage carries the bundled system libraries (if any) with their Debian copyright
    # files under third-party/, and its copyright file lists them. The .deb bundles none: it
    # depends on the system packages, whose own copyright files cover them.
    local doc="$1" kind="$2"
    mkdir -p "$doc"
    local note
    if [[ "$kind" == appimage ]] && ((${#bundled_sonames[@]})); then
        note="This AppImage bundles the system libraries listed below, each with the copyright file of its Debian package under /usr/share/doc/zxraw/third-party/."
    elif [[ "$kind" == appimage ]]; then
        note="This AppImage bundles no system libraries. It uses the host's C and C++ runtimes, and loads the host's X11, Wayland, Vulkan and GL libraries."
    else
        note="This package bundles no system libraries. The libraries it links to are package dependencies ($depends_line); their licenses are in /usr/share/doc/<package>/copyright of those packages."
    fi
    {
        # The note goes into the header paragraph (DEP-5 allows Comment there), after Source.
        local line wrapped
        while IFS= read -r line; do
            printf '%s\n' "$line"
            if [[ "$line" == Source:* ]]; then
                wrapped="$(fold -s -w 90 <<<"$note" | sed 's/ *$//')"
                printf 'Comment: %s\n' "$(head -n1 <<<"$wrapped")"
                tail -n +2 <<<"$wrapped" | sed 's/^/ /'
            fi
        done < "$copyright_src"
        if [[ "$kind" == appimage ]]; then
            local soname pkg
            for soname in "${bundled_sonames[@]}"; do
                pkg="${pkg_of_soname[$soname]%%:*}"
                echo
                echo "Files: usr/lib/$soname"
                echo "Copyright: see /usr/share/doc/zxraw/third-party/$pkg/copyright"
                echo "License: other"
                echo " As stated in /usr/share/doc/zxraw/third-party/$pkg/copyright, the unchanged"
                echo " copyright file of the Debian/Ubuntu package $pkg"
                echo " $(dpkg-query -W -f='${Version}' "${pkg_of_soname[$soname]}" 2>/dev/null || echo '(version unknown)'), from which the library was copied."
            done
        fi
    } > "$doc/copyright"
    chmod 644 "$doc/copyright"
    if [[ "$kind" == appimage ]]; then
        local soname pkg
        for soname in "${bundled_sonames[@]}"; do
            pkg="${pkg_of_soname[$soname]%%:*}"
            [[ -f "/usr/share/doc/$pkg/copyright" ]] ||
                die "no copyright file for bundled library $soname (package $pkg)"
            mkdir -p "$doc/third-party/$pkg"
            install -m644 "/usr/share/doc/$pkg/copyright" "$doc/third-party/$pkg/copyright"
        done
    fi
    install -m644 "$third_party_src" "$doc/THIRD_PARTY.md"
    install -m644 "$repo/LICENSE-MIT" "$doc/LICENSE-MIT"
    install -m644 "$repo/LICENSE-APACHE" "$doc/LICENSE-APACHE"
    install -m644 "$repo/packaging/LICENSE-UNRAR.txt" "$doc/LICENSE-UNRAR.txt"
    install -m644 "$repo/packaging/LICENSES-NOTE.md" "$doc/LICENSES-NOTE.md"
}

install_icons() {
    # Usage: install_icons <share dir>  (creates <share>/icons/hicolor)
    mkdir -p "$1/icons"
    cp -r "$icons_src" "$1/icons/"
    find "$1/icons" -type f -exec chmod 644 {} +
    find "$1/icons" -type d -exec chmod 755 {} +
}

fetch_verified() {
    # Usage: fetch_verified <url> <sha256> <file>
    # Downloads <url> to <file> unless it is already there, then aborts unless its SHA-256 is
    # <sha256>. A mismatching download is removed; a mismatching cached file is left for a look.
    local url="$1" want="$2" file="$3" got
    if [[ ! -f "$file" ]]; then
        mkdir -p "$(dirname "$file")"
        echo "Downloading $url ..."
        curl -fL --retry 3 -o "$file.part" "$url"
        got="$(sha256sum "$file.part" | cut -d' ' -f1)"
        if [[ "$got" != "$want" ]]; then
            rm -f "$file.part"
            die "SHA-256 mismatch for $url: expected $want, got $got"
        fi
        mv "$file.part" "$file"
    fi
    got="$(sha256sum "$file" | cut -d' ' -f1)"
    [[ "$got" == "$want" ]] || die "SHA-256 mismatch for cached $file: expected $want, got $got"
}

build_appimage() {
    local appdir="$work/AppDir"
    local tool_dir="${XDG_CACHE_HOME:-$HOME/.cache}/zxraw-packaging"
    local tool="$tool_dir/appimagetool-1.9.1-x86_64.AppImage"
    local runtime="$tool_dir/runtime-20251108-x86_64"

    mkdir -p "$appdir/usr/bin" "$appdir/usr/lib" "$appdir/usr/share/applications" \
        "$appdir/usr/share/doc/zxraw"
    install -m755 "$staged_binary" "$appdir/usr/bin/zxraw"
    install -m755 "$staged_cli" "$appdir/usr/bin/zxraw-cli"
    for soname in "${bundled_sonames[@]}"; do
        install -m644 "${linked_path[$soname]}" "$appdir/usr/lib/$soname"
    done
    install -m644 "$desktop_src" "$appdir/usr/share/applications/zxraw.desktop"
    install -m644 "$desktop_src" "$appdir/zxraw.desktop"
    install_icons "$appdir/usr/share"
    install -m644 "$icons_src/256x256/apps/zxraw.png" "$appdir/zxraw.png"
    ln -s zxraw.png "$appdir/.DirIcon"
    install_docs "$appdir/usr/share/doc/zxraw" appimage

    {
        echo '#!/bin/sh'
        echo '# Starts the bundled zxraw. The AppImage bin directory goes first on PATH, so the'
        echo '# app finds the same helpers as the installed package; bundled libraries, if any,'
        echo '# are added to LD_LIBRARY_PATH.'
        echo 'HERE="$(dirname "$(readlink -f "$0")")"'
        echo 'export PATH="$HERE/usr/bin:$PATH"'
        if ((${#bundled_sonames[@]})); then
            echo 'export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"'
        fi
        echo 'exec "$HERE/usr/bin/zxraw" "$@"'
    } > "$appdir/AppRun"
    chmod 755 "$appdir/AppRun"

    # Pinned and verified: cached, downloaded once, never installed, checked on every build.
    fetch_verified "$APPIMAGETOOL_URL" "$APPIMAGETOOL_SHA256" "$tool"
    fetch_verified "$RUNTIME_URL" "$RUNTIME_SHA256" "$runtime"
    chmod 755 "$tool"

    # Without FUSE (or libfuse2) the tool cannot mount itself, so extract and run it instead.
    local tool_env=(env ARCH=x86_64)
    if [[ ! -w /dev/fuse ]] || ! ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; then
        tool_env+=(APPIMAGE_EXTRACT_AND_RUN=1)
    fi
    echo "Creating $appimage_name ..."
    rm -f "$dist/$appimage_name"
    "${tool_env[@]}" "$tool" --runtime-file "$runtime" "$appdir" "$dist/$appimage_name"
    chmod 755 "$dist/$appimage_name"
}

build_deb() {
    local root="$work/deb"
    local maintainer installed_kib
    mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/applications" \
        "$root/usr/share/doc/zxraw"
    install -m755 "$staged_binary" "$root/usr/bin/zxraw"
    install -m755 "$staged_cli" "$root/usr/bin/zxraw-cli"
    install -m644 "$desktop_src" "$root/usr/share/applications/zxraw.desktop"
    install_icons "$root/usr/share"
    install_docs "$root/usr/share/doc/zxraw" deb

    maintainer="$(git -C "$repo" log --reverse --format='%an <%ae>' 2>/dev/null | head -n1 || true)"
    [[ -n "$maintainer" ]] || maintainer="ZXraw Explorer developers"
    installed_kib="$(du -sk "$root/usr" | cut -f1)"
    {
        echo "Package: zxraw"
        echo "Version: $VERSION"
        echo "Section: graphics"
        echo "Priority: optional"
        echo "Architecture: amd64"
        echo "Maintainer: $maintainer"
        echo "Installed-Size: $installed_kib"
        echo "Depends: $depends_line"
        [[ -n "$recommends_line" ]] && echo "Recommends: $recommends_line"
        echo "Suggests: ffmpeg"
        echo "Homepage: https://github.com/zuhuHix/zxraw-explorer"
        echo "Description: photo and RAW file explorer with a built-in viewer"
        echo " ZXraw Explorer is a fast, free, local-first file manager for photographers and"
        echo " videographers. It browses folders of photos and RAW files, shows them in a"
        echo " built-in viewer, and uses ffmpeg for video thumbnails when it is installed."
    } > "$root/DEBIAN/control"
    chmod 644 "$root/DEBIAN/control"

    find "$root" -type d -exec chmod 755 {} +
    echo "Creating $deb_name ..."
    rm -f "$dist/$deb_name"
    dpkg-deb --build --root-owner-group "$root" "$dist/$deb_name" >/dev/null
}

build_appimage
build_deb

(cd "$dist" && sha256sum "$appimage_name" "$deb_name" > SHA256SUMS-linux.txt)

echo
echo "Release files in $dist:"
ls -l "$dist/$appimage_name" "$dist/$deb_name" "$dist/SHA256SUMS-linux.txt"
echo
cat "$dist/SHA256SUMS-linux.txt"

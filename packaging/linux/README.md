# Linux packaging

Builds the Linux release files: an AppImage and a `.deb` for amd64, plus `SHA256SUMS-linux.txt`.
Nothing here needs root, system-wide installs or pwsh; the Windows installer and zip are built by
`packaging/release.ps1` as before.

| File | What it is |
| --- | --- |
| `build-linux.sh` | Release build (`cargo build --release --no-default-features -p forge-app -p forge-cli`), then the AppImage (`zxraw` and `zxraw-cli`, as in the Windows zip), the `.deb` and the checksums in `dist/`. `--skip-build` packages the existing binary. |
| `../release-linux.sh` | `release-linux.sh <x.y.z>`: checks the tree version, runs `build-linux.sh`, prints the `gh release upload` command. Never uploads, tags or pushes. |
| `zxraw.desktop` | Desktop entry, shipped in both packages. See *Desktop entry* below. |
| `make-icons.sh`, `icons/hicolor/` | hicolor PNGs made from `assets/zxraw.png`. The output is committed, so builds need none of the tools. |
| `copyright` | DEP-5 copyright file shipped as `/usr/share/doc/zxraw/copyright`. |
| `generate-third-party.sh`, `THIRD_PARTY-linux.md` | The Linux-target counterpart of `packaging/THIRD_PARTY.md`. Shipped as `THIRD_PARTY.md` in both packages. |

## Build

```sh
packaging/linux/build-linux.sh                 # build, then dist/ (needs ~5 min on 12 cores)
packaging/linux/build-linux.sh --skip-build    # package the existing target/release/zxraw and zxraw-cli
CARGO_TARGET_DIR=/some/dir packaging/linux/build-linux.sh
```

The version comes from `[workspace.package]` in `Cargo.toml`. The `.deb` build uses `dpkg-deb`
directly, so `cargo-deb` is not needed. The AppImage build downloads pinned releases of
`appimagetool` and the AppImage runtime into `${XDG_CACHE_HOME:-$HOME/.cache}/zxraw-packaging/`
once and reuses them (see *AppImage tools* below). Nothing is installed. `dist/` is git-ignored.
The script builds x86_64/amd64 packages only and stops on any other host (`uname -m`).

Outputs (version `0.10.0` shown):

* `dist/ZXraw-Explorer-0.10.0-x86_64.AppImage`
* `dist/zxraw_0.10.0_amd64.deb`
* `dist/SHA256SUMS-linux.txt` (`sha256sum` format, both files)

Tools needed on the build machine: `cargo`, `python3` (3.11+, for `tomllib`), `ldd`, `objdump`,
`dpkg`, `dpkg-deb`, `strip`, `curl`, `sha256sum`. `desktop-file-validate` is optional and only used
by hand (see *Checks*).

## Decisions

**Shared libraries.** `ldd` on the release binary lists only `libc`, `libm`, `libgcc_s` and
`libstdc++`. The windowing and GPU libraries (X11, Wayland, xkbcommon, Vulkan, GL) are loaded with
`dlopen` at run time, so they do not appear in `ldd`. Because of that, the AppImage bundles
nothing. Any linked library that is not on the host list in `build-linux.sh` is copied into the
AppImage and `AppRun` sets `LD_LIBRARY_PATH` for it. Bundling the desktop and GPU libraries is not
done: the AppImage would then carry drivers and libraries that must match the host.

**`.deb` dependencies** come from `ldd`, each library mapped to its package with `dpkg -S`. The
current build gives:

```
Depends: libc6 (>= 2.39), libgcc-s1, libstdc++6
Recommends: libvulkan1 | libgl1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libx11-6, xdg-utils
Suggests: ffmpeg
```

`libc6 (>= 2.39)` is the highest `GLIBC_` symbol version the binary needs, read with `objdump -T`.
The Recommends list is the runtime-loaded set. An entry is dropped from it if a Depends already
covers it.

**Stripping.** The release profile keeps line tables, so the binary is 227 MB. The packaged copy
is stripped with `strip --strip-debug`, which gives 53 MB. The symbol table stays, so backtraces
keep function names. `target/release/zxraw` is not changed.

**AppImage tools.** Both are exact releases, checked against a SHA-256 written in `build-linux.sh`
before every run (also when cached). A mismatch stops the build. To update, change the URL and the
hash together; GitHub shows each asset's `sha256` digest on its release page.

| Tool | Release | SHA-256 |
| --- | --- | --- |
| `appimagetool-x86_64.AppImage` | AppImage/appimagetool 1.9.1 | `ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0` |
| `runtime-x86_64` | AppImage/type2-runtime 20251108 | `2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d` |

The runtime is passed with `--runtime-file`, so `appimagetool` downloads nothing itself. linuxdeploy
is not used. The tool runs with `APPIMAGE_EXTRACT_AND_RUN=1` when FUSE or `libfuse2` is missing.

**Licenses of system libraries.** The crate inventory (`THIRD_PARTY.md`) covers the Rust code. A
system library copied into the AppImage (none today; for example `libheif` if the `heif` feature
links it) is shipped with its package's `/usr/share/doc/<package>/copyright`, unchanged, under
`/usr/share/doc/zxraw/third-party/<package>/`. The generated `copyright` file has one stanza per
bundled library. The `.deb` never bundles: linked libraries are package dependencies, and the
`copyright` file says so.

## Desktop entry

`zxraw.desktop` has `Exec=zxraw %f`: the app takes a plain path (not a URI) and opens only the
first one, so the desktop launches it once per selected file.

**MIME types are an optional handler.** The `MimeType=` list makes ZXraw Explorer appear in
*Open With* for the photo and RAW types the viewer opens, and for folders (`inode/directory`).
Installing the package does not make it the default for any type. The user chooses it, or sets it
with `xdg-mime`. The list is `IMAGE_EXTS` plus `RAW_EXTS` from `crates/forge-core/src/model.rs`,
minus the types the viewer cannot decode (`psd`, `jxl`, `ico`, `crw`, `x3f`, `mrw`, see
`NOT_VIEWABLE` in `crates/forge-app/src/default_app.rs`). Seven RAW extensions (`srw`, `3fr`,
`iiq`, `erf`, `mef`, `mos`, `rwl`) have no shared-mime-info type on Ubuntu 24.04, so they are not
listed.

**Window class.** On X11, winit uses the file name of `argv[0]` as `WM_CLASS`, so the window
class is `zxraw`, which matches `StartupWMClass=zxraw`. On Wayland the app sets the app id `zxraw`
(`with_app_id` in `crates/forge-app/src/main.rs`, Linux only), so docks link the window to this
desktop file too.

## Icons

`assets/zxraw.png` is 256 × 256, so the set is 16, 24, 32, 48, 64, 128 and 256 px. No 512 px icon is
made because the source would have to be upscaled. `make-icons.sh` uses ImageMagick if it is
installed, then `python3 -I` with Pillow, then ffmpeg. Run it again only when the master icon
changes.

## Checks

```sh
desktop-file-validate packaging/linux/zxraw.desktop    # passes with no hints
dpkg-deb --info dist/zxraw_*_amd64.deb
dpkg-deb --contents dist/zxraw_*_amd64.deb
(cd dist && sha256sum -c SHA256SUMS-linux.txt)
```

`lintian` is not installed on the build machine, so it has not been run. The AppImage build
warns that AppStream metadata is missing (`usr/share/metainfo/zxraw.appdata.xml`). That is a
cosmetic gap for the AppImage tool, not a failure.

## Linux release steps

The Windows release comes first. The Linux files are added to the same GitHub release.

1. Bump the version as on Windows (`[workspace.package] version` in `Cargo.toml`) and commit it as
   "Release x.y.z". The Windows `packaging/release.ps1` does the bump; on Linux, bump it by hand.
2. `packaging/release-linux.sh x.y.z` builds and checks the version. It prints the upload command.
3. Tag and push the commit, then `gh release create` for the Windows files (see
   `packaging/release.ps1`), or let the release exist already.
4. Run the printed `gh release upload v<x.y.z> dist/... --clobber` command. Check the printed
   `SHA256SUMS-linux.txt` against the files first.
5. Keep the Linux checksums in their own file. The Windows in-app updater reads `SHA256SUMS.txt`;
   `SHA256SUMS-linux.txt` is separate and is not read by it.

Nothing in this folder publishes anything by itself.

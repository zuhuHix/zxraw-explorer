<p align="center">
  <img src="assets/zxraw.png" width="96" alt="ZXraw Explorer logo">
</p>

<h1 align="center">ZXraw Explorer</h1>

<p align="center">
  <b>The file explorer for people who shoot.</b><br>
  A fast, free, local-first Windows file manager for photographers and videographers.
</p>

<p align="center">
  <img src="docs/images/hero.jpg" alt="Photos and film frames from a memory card flowing through a camera aperture into organised folders">
</p>

> **Status: early development.** Everything below is built, but not all of it is battle-tested
> on real camera files yet. [docs/FEATURES.md](docs/FEATURES.md) lists exactly what is verified
> and how, and [docs/LIMITATIONS.md](docs/LIMITATIONS.md) lists the known gaps.

---

## Why not just use File Explorer?

<p align="center">
  <img src="docs/images/problem.jpg" alt="Left: grey placeholder tiles waiting to load. Right: the same grid as sharp photo thumbnails, some starred">
</p>

File Explorer is built to open documents. A shoot is something else: thousands of near-identical
RAW files, a memory card you can't afford to lose, and an evening of picking the keepers. ZXraw
Explorer is a file manager designed around that workflow:

| | File Explorer | ZXraw Explorer |
|---|:---:|:---:|
| RAW thumbnails and previews (CR2, CR3, NEF, ARW, DNG, RAF, ORF…) | needs an extension | ✅ built in |
| Two folders side by side, with tabs | tabs only | ✅ |
| Rate, pick, reject and label photos with single keys | — | ✅ |
| Group bursts and look-alike shots into stacks | — | ✅ |
| Import from a camera card with checksum-verified copies | — | ✅ |
| Batch rename with a live preview and undo | — | ✅ |
| Filters like `ext:cr3 rating:>=4 lens:35mm` | limited | ✅ |
| Duplicate finder, folder compare, folder size analysis | — | ✅ |
| Command palette for every action | — | ✅ |
| Accounts, telemetry, cloud | built in | **none** |

---

## What makes it different

<table>
  <tr>
    <td align="center" width="33%">
      <img src="docs/images/pillar-speed.jpg" width="120" alt=""><br>
      <b>Fast with huge folders</b><br>
      Folders stream in as they're read and thumbnails are cached on disk, so even a
      50,000-file folder opens without waiting for the whole listing.
    </td>
    <td align="center" width="33%">
      <img src="docs/images/pillar-raw.jpg" width="120" alt=""><br>
      <b>RAW-native</b><br>
      Thumbnails and previews come straight from the embedded previews in your RAW files.
      No codec packs to install.
    </td>
    <td align="center" width="33%">
      <img src="docs/images/pillar-cull.jpg" width="120" alt=""><br>
      <b>Built for culling</b><br>
      A full-screen viewer with compare mode, a filmstrip and one-key ratings, picks and
      rejects. Bursts collapse into stacks.
    </td>
  </tr>
  <tr>
    <td align="center">
      <img src="docs/images/pillar-safe.jpg" width="120" alt=""><br>
      <b>Careful with your files</b><br>
      Copies are written under a temporary name and only appear when complete. Imports are
      verified with BLAKE3 checksums. Nothing is silently overwritten.
    </td>
    <td align="center">
      <img src="docs/images/pillar-organize.jpg" width="120" alt=""><br>
      <b>Find anything</b><br>
      Tags, collections and smart collections, plus search across camera, lens, ISO, rating,
      date and more. Ratings can also be written to XMP sidecars.
    </td>
    <td align="center">
      <img src="docs/images/pillar-video.jpg" width="120" alt=""><br>
      <b>Video too</b><br>
      Clip thumbnails, technical metadata, GoPro chapter grouping and proxy files, using FFmpeg.
      The app can download FFmpeg for you in one click.
    </td>
  </tr>
</table>

---

## Import from a card, safely

<p align="center">
  <img src="docs/images/ingest.jpg" alt="An SD card sending photos through a verification checkpoint into tidy folders">
</p>

When you plug in a camera card, ZXraw Explorer offers to import it:

- It plans the copy first, using your own folder template (for example `2026/10-12 Lisbon`), and
  shows exactly what will go where.
- It copies, then **re-reads every file and compares checksums** before calling it done.
- It **never modifies the card.**
- Run the import again and it recognises what's already there, so nothing is duplicated.

## Local and private

<p align="center">
  <img src="docs/images/privacy.jpg" width="720" alt="A laptop sheltered under a protective dome, cut off from network lines">
</p>

- No account and no telemetry.
- Your ratings, tags, index and thumbnail cache live in a local database on your PC.
- The app makes **no network connections at all**, with one exception: the optional FFmpeg
  download, which only runs when you click it and confirm, and is checked against a pinned
  checksum.

---

## Download

Requirements: Windows 10 or 11 (64-bit).

- **Installer:** `ZXraw-Explorer-<version>-Setup-x64.exe`. Installs for your user only, so no
  admin rights are needed. Optionally adds *Open in ZXraw Explorer* to folder right-click menus.
- **Portable:** `ZXraw-Explorer-<version>-x64.zip`. Unzip anywhere and run `zxraw.exe`.

Builds will be published on the [Releases](../../releases) page. The app isn't code-signed yet,
so Windows SmartScreen may warn the first time: choose *More info → Run anyway*.

---

## Support the project

<p align="center">
  <img src="docs/images/support.jpg" width="720" alt="A coffee cup with latte art shaped like a camera aperture, next to an SD card and a camera">
</p>

**ZXraw Explorer is free and will stay free.** No paid tier, no ads, no locked features. If it saves
you time on your next shoot, you can support its development. Even starring the repo or telling a
photographer friend helps.

<!-- Donation links go here once the account is set up, e.g.
[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-2F6FEB)](https://github.com/sponsors/USERNAME)
-->

---

## For developers

Requirements: Windows 10/11 x64, Rust ≥ 1.90 (MSVC toolchain).

```powershell
cargo build --release          # target\release\zxraw.exe and zxraw-cli.exe
cargo test --workspace
cargo run -p forge-app         # launch the UI (debug)
packaging\build-release.ps1    # tested release build + portable zip in dist\
```

The installer is built from `packaging\zxraw.iss` with Inno Setup 6.

Optional: video thumbnails and technical metadata need `ffmpeg.exe` and `ffprobe.exe`.
ZXraw Explorer does not bundle FFmpeg. Use **Download FFmpeg…** (in Settings › Video, the
welcome window or the command palette) to fetch a pinned, checksum-verified build into the
app's data folder. Alternatively, put the tools on `PATH` or choose their folder in Settings.

### Docs

* [Architecture](docs/ARCHITECTURE.md)
* [Plan / TODO](docs/PLAN.md)
* [Feature checklist](docs/FEATURES.md): what's verified, and by which test
* [Testing & benchmarks](docs/TESTING.md)
* [Known limitations](docs/LIMITATIONS.md)

# Forge

A fast, local-first Windows file manager in Rust, built for photographers, videographers
and everyone else who lives in File Explorer.

> Status: early development. See [docs/FEATURES.md](docs/FEATURES.md) for what actually
> works today and [docs/LIMITATIONS.md](docs/LIMITATIONS.md) for known gaps.

## Build

Requirements: Windows 10/11 x64, Rust ≥ 1.90 (MSVC toolchain).

```powershell
cargo build --release          # target\release\forge.exe and forge-cli.exe
cargo test --workspace
cargo run -p forge-app         # launch the UI (debug)
```

Optional: put `ffmpeg.exe` and `ffprobe.exe` on `PATH` (or configure their location in
Settings) to enable video thumbnails and technical metadata. Forge does not bundle FFmpeg.

## Docs

* [Architecture](docs/ARCHITECTURE.md)
* [Plan / TODO](docs/PLAN.md)
* [Feature checklist](docs/FEATURES.md)
* [Testing & benchmarks](docs/TESTING.md)
* [Known limitations](docs/LIMITATIONS.md)

## Privacy

No accounts, no telemetry, no network access. Indexes and caches stay on your machine.

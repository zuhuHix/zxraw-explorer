# ZXraw Explorer: notes for coding agents

- Rust workspace (`crates/forge-*`); the app is `crates/forge-app` (egui, Windows only). Build
  with `CARGO_PROFILE_DEV_DEBUG=0` to keep target directories small.
- **Testing:** while iterating, run `scripts/test-affected.ps1` (on Linux:
  `scripts/test-affected.sh`, same logic, `--dry-run`). It runs only the crates and e2e
  areas your change reaches, and `-DryRun` shows the plan. Run the full `cargo test --workspace`
  once, before opening a PR or releasing. Details are in [docs/TESTING.md](docs/TESTING.md).
- `related::video_proxy_conventions_and_external_root_scan_are_visible` used to time out (about 1
  run in 5) while the preview was still loading; it now waits for `ui related wait-preview-settled`.
  If it fails again, rerun it alone and report it.
- Never kill the `zxraw.exe` under `AppData\Local\Programs`: it is the user's installed copy.
- Releases: `packaging/release.ps1 -Version x.y.z`, then commit "Release x.y.z", tag `vx.y.z`,
  push, and `gh release create` with user-facing notes and the three `dist` files.
- Linux e2e: every app run gets its own `Xvfb` (needs `xvfb` and Mesa). Never kill an `Xvfb` or
  `zxraw` you did not start; `:0` is the user's display.

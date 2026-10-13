# ZXraw Explorer: notes for coding agents

- Rust workspace (`crates/forge-*`); the app is `crates/forge-app` (egui, Windows only). Build
  with `CARGO_PROFILE_DEV_DEBUG=0` to keep target directories small.
- **Testing:** while iterating, run `scripts/test-affected.ps1`. It runs only the crates and e2e
  areas your change reaches, and `-DryRun` shows the plan. Run the full `cargo test --workspace`
  once, before opening a PR or releasing. Details are in [docs/TESTING.md](docs/TESTING.md).
- `related::video_proxy_conventions_and_external_root_scan_are_visible` is timing-flaky (about 1
  run in 5 times out). Rerun it alone before treating it as a failure.
- Never kill the `zxraw.exe` under `AppData\Local\Programs`: it is the user's installed copy.
- Releases: `packaging/release.ps1 -Version x.y.z`, then commit "Release x.y.z", tag `vx.y.z`,
  push, and `gh release create` with user-facing notes and the three `dist` files.

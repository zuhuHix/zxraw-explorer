# Releasing ZXraw Explorer

Releases are built by GitHub Actions (`.github/workflows/release.yml`) from a version tag. The
workflow makes a **draft** release. A person checks it and publishes it.

## Steps

1. **Set the version.** On Linux or macOS: `packaging/bump-version.sh x.y.z` (add `--dry-run`
   to see what changes first). On Windows: `packaging\release.ps1 -Version x.y.z -SkipTests`
   does the same bump and also builds locally. Both set `[workspace.package] version` in
   `Cargo.toml` and `#define AppVersion` in `packaging/zxraw.iss`, and refresh the workspace
   entries in `Cargo.lock`. Running either again with the same version changes nothing.
2. **Commit, tag, push.**

   ```sh
   git commit -am "Release x.y.z"
   git tag vx.y.z
   git push origin HEAD vx.y.z
   ```

3. **The Release workflow builds a draft.** The tag push starts it. It:
   * checks that the tag (`vx.y.z`) equals the `Cargo.toml` workspace version, and stops if not;
   * **Windows** (`windows-latest`): runs the workspace tests that ci.yml's Windows job runs,
     installs Inno Setup 6.7.4 if the image has no Inno Setup 6 (pinned, SHA-256 checked), then
     `packaging/release.ps1 -Version x.y.z -SkipTests`, and checks that its version bump was a
     no-op. Output: `ZXraw-Explorer-x.y.z-x64.zip`, `ZXraw-Explorer-x.y.z-Setup-x64.exe`,
     `SHA256SUMS.txt`;
   * **Linux** (`ubuntu-24.04`): `packaging/linux/build-linux.sh`. Output:
     `ZXraw-Explorer-x.y.z-x86_64.AppImage`, `zxraw_x.y.z_amd64.deb`, `SHA256SUMS-linux.txt`;
   * **Publish**: checks the six files against both checksum files, then creates the draft release
     `vx.y.z` with them (or replaces the files of an existing draft). It refuses to touch a release
     that is already published, and never creates a tag.

   Release builds start from a clean cache (no `rust-cache`), so they take longer than CI.
4. **Check the draft and publish it.** Download the files from the draft, check them, write the
   user-facing notes, then:

   ```sh
   gh release edit vx.y.z --draft=false --notes-file notes.md
   ```

   The in-app update check sees only published, non-draft, non-prerelease releases, and refuses an
   installer that `SHA256SUMS.txt` does not list. `SHA256SUMS-linux.txt` is separate and is not
   read by the Windows updater.

## Running the workflow by hand

*Actions › Release › Run workflow* (or `gh workflow run release.yml`) takes:

* `tag`: an existing `vx.y.z` tag. The workflow builds that tag and drafts its release, as a push
  would.
* `dry_run`: build the branch the workflow is run on instead of the tag. The version check still
  compares `Cargo.toml` with `tag`, the files are kept only as run artifacts
  (`release-windows`, `release-linux`, 14 days), and no release or tag is created or changed. Use
  it to test the workflow:

  ```sh
  gh workflow run release.yml --ref <branch> -f tag=v<version in Cargo.toml> -f dry_run=true
  ```

## Building locally instead

* Windows: `packaging\release.ps1 -Version x.y.z` builds and tests everything and prints the
  `gh release create` command (see its header).
* Linux: `packaging/release-linux.sh x.y.z` builds the Linux files for a version already set in
  the tree and prints the `gh release upload` command. See `packaging/linux/README.md`.

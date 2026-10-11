# Third-party notices

The workspace does not currently have `cargo-about` installed, so this file is a
manually maintained summary. To generate a full dependency inventory, install
`cargo-about`, then run from the repository root:

```powershell
cargo about init
cargo about generate about.hbs -o packaging/THIRD_PARTY.md
```

Key direct dependencies and their declared SPDX licenses, checked against the
crate manifests used by this workspace:

| Dependency | Declared license |
| --- | --- |
| eframe / egui | MIT OR Apache-2.0 |
| image | MIT OR Apache-2.0 |
| rusqlite | MIT |
| windows | MIT OR Apache-2.0 |
| clap | MIT OR Apache-2.0 |
| notify | CC0-1.0 |
| zip | MIT |
| embed-resource | MIT |

This is not a complete notice for every transitive dependency. Regenerate the
full inventory for a release. FFmpeg and ffprobe are optional external programs;
ZXraw Explorer does not bundle them.

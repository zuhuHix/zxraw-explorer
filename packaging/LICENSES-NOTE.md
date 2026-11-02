# License notes

ZXraw Explorer is distributed under the MIT License or Apache License 2.0; both
are included with the release. The complete Windows-target inventory of runtime
dependencies, their declared license expressions, repositories, and review flags
is in THIRD_PARTY.md.

Dependencies marked for review need a compatible redistribution decision before
release. The report calls out MPL-2.0 as weak copyleft and flags licenses outside
the permissive allowlist. ZXraw Explorer does not bundle FFmpeg or ffprobe (the in-app
download fetches them from their publisher only when the user asks); they
remain optional external programs discovered on PATH or configured by the user.

RAR support uses the freeware UnRAR source code by Alexander Roshal, under the UnRAR licence
(full text in THIRD_PARTY.md and in LICENSE-UNRAR.txt). The licence allows the source to be used
to handle RAR archives, but not to develop a RAR (WinRAR) compatible archiver or to re-create the
RAR compression algorithm. ZXraw Explorer only opens and extracts RAR archives; it does not create
them.

<#
.SYNOPSIS
Runs the tests affected by the current changes: unit tests for the changed crates and the
crates that depend on them, plus the e2e areas that the changed app files reach.

.DESCRIPTION
Changes are the files that differ from the merge-base of HEAD and -Base (default origin/main),
plus uncommitted and untracked files. Unit tests (lib, bin and integration tests except e2e)
run for every crate with a changed file and every crate that depends on it, directly or not
(from `cargo metadata`). E2E areas come from the table below. E2E is skipped when only docs,
*.md, assets, packaging or scripts changed. The plan is printed before anything runs.

Exit code: 0 when everything ran and passed (or nothing was affected), 1 when a step failed.

.PARAMETER Base
Ref to compare against (default origin/main). Example: -Base HEAD~3

.PARAMETER All
Run the whole workspace: cargo test --workspace --no-fail-fast (includes e2e).

.PARAMETER DryRun
Print the plan and the cargo commands, run nothing.

.EXAMPLE
powershell -File scripts\test-affected.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [string]$Base = 'origin/main',
    [switch]$All,
    [switch]$DryRun
)

# ---------------------------------------------------------------------------------------
# E2E map: changed file -> e2e areas (e2e modules in crates/forge-app/tests/e2e).
# Paths are relative to the repo root and use -like wildcards. Every matching row adds its
# areas. 'all' means every area. A forge-app file that matches no row runs every area.
# Areas: browser, core, ingest, library, media, perf, related, sidebar, system, tools, viewer.
#
# Shared core (app shell, settings, commands, theme, test hook): core only, except where a
# row names the area that uses the file (settings_ui.rs: media-link rules are in related;
# testhook/<area>.rs belongs to its area).
# forge-core and forge-jobs reach every feature, so they run all areas. forge-db, forge-media
# and forge-win run the areas whose features use them (a judgement call; widen a row when in
# doubt).
# ---------------------------------------------------------------------------------------
$E2EMap = @(
    # shared core
    @('crates/forge-app/src/app.rs', 'core'),
    @('crates/forge-app/src/main.rs', 'core'),
    @('crates/forge-app/src/commands.rs', 'core'),
    @('crates/forge-app/src/theme.rs', 'core'),
    @('crates/forge-app/src/settings.rs', 'core'),
    @('crates/forge-app/src/settings_ui.rs', 'core,related'),
    @('crates/forge-app/src/actions.rs', 'core'),
    @('crates/forge-app/src/modal.rs', 'core'),
    @('crates/forge-app/src/palette.rs', 'core'),
    @('crates/forge-app/src/dialogs.rs', 'core'),
    @('crates/forge-app/src/testhook_off.rs', 'core'),
    @('crates/forge-app/src/testhook/*', 'core'),
    @('crates/forge-app/src/testhook/browser.rs', 'browser'),
    @('crates/forge-app/src/testhook/ingest.rs', 'ingest'),
    @('crates/forge-app/src/testhook/library.rs', 'library'),
    @('crates/forge-app/src/testhook/media.rs', 'media'),
    @('crates/forge-app/src/testhook/perf.rs', 'perf'),
    @('crates/forge-app/src/testhook/related.rs', 'related'),
    @('crates/forge-app/src/testhook/sidebar.rs', 'sidebar'),
    @('crates/forge-app/src/testhook/system.rs', 'system'),
    @('crates/forge-app/src/testhook/tools.rs', 'tools'),
    @('crates/forge-app/src/testhook/viewer.rs', 'viewer'),
    # library, search, collections
    @('crates/forge-app/src/collections_ui.rs', 'library'),
    @('crates/forge-app/src/library.rs', 'library'),
    @('crates/forge-app/src/search_ui.rs', 'library'),
    @('crates/forge-app/src/tags_ui.rs', 'library'),
    @('crates/forge-app/src/meta_edit.rs', 'library,media'),
    # viewer
    @('crates/forge-app/src/viewer/*', 'viewer'),
    @('crates/forge-app/src/viewer_host.rs', 'viewer'),
    # ingest
    @('crates/forge-app/src/ingest_ui.rs', 'ingest'),
    # media and related
    @('crates/forge-app/src/video_ui.rs', 'media,related'),
    @('crates/forge-app/src/media_meta.rs', 'media'),
    @('crates/forge-app/src/media_durations.rs', 'media'),
    @('crates/forge-app/src/gps_ui.rs', 'media'),
    @('crates/forge-app/src/meta_export.rs', 'media'),
    @('crates/forge-app/src/export_assoc.rs', 'related'),
    # tools
    @('crates/forge-app/src/tools_ui.rs', 'tools'),
    @('crates/forge-app/src/extra_tools_ui.rs', 'tools'),
    @('crates/forge-app/src/batch_export.rs', 'tools'),
    @('crates/forge-app/src/batch_rename.rs', 'tools,core'),
    @('crates/forge-app/src/ops_ui.rs', 'core,tools'),
    @('crates/forge-app/src/watcher.rs', 'core,tools'),
    # browser (panes, views, sidebar, grouping)
    @('crates/forge-app/src/views.rs', 'browser,perf'),
    @('crates/forge-app/src/pane.rs', 'browser,core'),
    @('crates/forge-app/src/sidebar*.rs', 'browser'),
    @('crates/forge-app/src/breadcrumb.rs', 'browser'),
    # sidebar: Quick access pins and saved layouts
    @('crates/forge-app/src/sidebar.rs', 'sidebar'),
    @('crates/forge-app/src/sidebar/*', 'sidebar'),
    @('crates/forge-app/src/layouts.rs', 'sidebar'),
    @('crates/forge-app/src/path_probe.rs', 'sidebar'),
    @('crates/forge-app/src/similar.rs', 'browser'),
    @('crates/forge-app/src/platform.rs', 'browser'),
    # thumbnails and previews are used by the grid everywhere
    @('crates/forge-app/src/thumbs.rs', 'core,viewer,perf'),
    @('crates/forge-app/src/preview.rs', 'core,viewer,perf'),
    # core-only leaf modules
    @('crates/forge-app/src/dragout.rs', 'core'),
    @('crates/forge-app/src/crash.rs', 'core'),
    @('crates/forge-app/src/notices.rs', 'core'),
    @('crates/forge-app/src/fonts.rs', 'core'),
    @('crates/forge-app/src/app_icon.rs', 'core'),
    @('crates/forge-app/src/default_app.rs', 'core,system'),
    # system: updater and ffmpeg download
    @('crates/forge-app/src/updater.rs', 'system'),
    @('crates/forge-app/src/ffmpeg_setup.rs', 'system'),
    # e2e harness and area modules
    @('crates/forge-app/tests/e2e/main.rs', 'all'),
    @('crates/forge-app/tests/e2e/browser.rs', 'browser'),
    @('crates/forge-app/tests/e2e/core.rs', 'core'),
    @('crates/forge-app/tests/e2e/ingest.rs', 'ingest'),
    @('crates/forge-app/tests/e2e/library.rs', 'library'),
    @('crates/forge-app/tests/e2e/media.rs', 'media'),
    @('crates/forge-app/tests/e2e/perf.rs', 'perf'),
    @('crates/forge-app/tests/e2e/related.rs', 'related'),
    @('crates/forge-app/tests/e2e/sidebar.rs', 'sidebar'),
    @('crates/forge-app/tests/e2e/system.rs', 'system'),
    @('crates/forge-app/tests/e2e/tools.rs', 'tools'),
    @('crates/forge-app/tests/e2e/viewer.rs', 'viewer'),
    # crates that forge-app links
    @('crates/forge-core/*', 'all'),
    @('crates/forge-jobs/*', 'all'),
    @('crates/forge-db/*', 'core,library,viewer,ingest,related'),
    @('crates/forge-media/*', 'core,media,viewer,related,system'),
    @('crates/forge-win/*', 'core,library,system,tools')
)

$AllAreas = @('browser', 'core', 'ingest', 'library', 'media', 'perf', 'related', 'sidebar', 'system', 'tools', 'viewer')

function Expand-Areas([string]$Spec) {
    if ($Spec -eq 'all') { return $AllAreas }
    return @($Spec.Split(',') | ForEach-Object { $_.Trim() })
}

function Test-NonCode([string]$File) {
    return ($File -like 'docs/*' -or $File -like 'assets/*' -or $File -like 'packaging/*' -or
        $File -like 'scripts/*' -or $File -like '*.md' -or $File -like 'LICENSE*' -or $File -eq '.gitignore')
}

function Get-GitLines([string[]]$GitArgs) {
    $out = & git @GitArgs
    if ($LASTEXITCODE -ne 0) { throw "git $($GitArgs -join ' ') failed" }
    return @($out | Where-Object { $_ -ne '' })
}

function Add-Item([hashtable]$Table, [string]$Key, [string]$Value) {
    if (-not $Table.ContainsKey($Key)) { $Table[$Key] = @() }
    if ($Table[$Key] -notcontains $Value) { $Table[$Key] += $Value }
}

# ---- repo, base and changed files ----------------------------------------------------------
$root = @(Get-GitLines @('rev-parse', '--show-toplevel'))[0].Replace('\', '/').TrimEnd('/')
Set-Location -LiteralPath $root

$mergeBase = @(Get-GitLines @('merge-base', 'HEAD', $Base))[0]
$tracked = @(Get-GitLines @('diff', '--name-only', $mergeBase))
$untracked = @(Get-GitLines @('ls-files', '--others', '--exclude-standard'))
$changed = @(@($tracked) + @($untracked) | Sort-Object -Unique)

# ---- workspace crates from cargo metadata -------------------------------------------------
$metaText = & cargo metadata --format-version 1 --no-deps
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
$meta = ($metaText -join "`n") | ConvertFrom-Json

$members = @{}
foreach ($p in $meta.packages) {
    $dir = $p.manifest_path.Replace('\', '/')
    $dir = $dir.Substring(0, $dir.LastIndexOf('/'))
    if (-not $dir.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) { continue }
    $rel = $dir.Substring($root.Length).TrimStart('/')
    $lib = $false; $bin = $false; $tests = @()
    foreach ($t in $p.targets) {
        $kinds = @($t.kind)
        if ($kinds -contains 'lib' -or $kinds -contains 'rlib' -or $kinds -contains 'proc-macro') { $lib = $true }
        if ($kinds -contains 'bin') { $bin = $true }
        if ($kinds -contains 'test' -and $t.name -ne 'e2e') { $tests += $t.name }
    }
    $deps = @($p.dependencies | Where-Object { $_.path } | ForEach-Object { $_.name })
    $members[$p.name] = [pscustomobject]@{
        Name = $p.name; Dir = $rel; Lib = $lib; Bin = $bin; Tests = $tests; Deps = $deps
    }
}

$dependents = @{}
foreach ($m in $members.Values) {
    foreach ($d in $m.Deps) {
        if ($members.ContainsKey($d)) { Add-Item $dependents $d $m.Name }
    }
}

# Crates that depend (directly or not) on the seeds, seeds included.
function Get-Dependents([string[]]$Seeds) {
    $seen = @{}
    $queue = New-Object System.Collections.Queue
    foreach ($s in $Seeds) { $seen[$s] = $true; $queue.Enqueue($s) }
    while ($queue.Count -gt 0) {
        $c = $queue.Dequeue()
        if ($dependents.ContainsKey($c)) {
            foreach ($d in $dependents[$c]) {
                if (-not $seen.ContainsKey($d)) { $seen[$d] = $true; $queue.Enqueue($d) }
            }
        }
    }
    return @($seen.Keys)
}

# Crates that the seed uses (directly or not), seed included.
function Get-Dependencies([string]$Seed) {
    $seen = @{ $Seed = $true }
    $queue = New-Object System.Collections.Queue
    $queue.Enqueue($Seed)
    while ($queue.Count -gt 0) {
        $c = $queue.Dequeue()
        foreach ($d in $members[$c].Deps) {
            if ($members.ContainsKey($d) -and -not $seen.ContainsKey($d)) { $seen[$d] = $true; $queue.Enqueue($d) }
        }
    }
    return @($seen.Keys)
}

function Find-Owner([string]$File) {
    $best = $null
    foreach ($m in $members.Values) {
        if ($m.Dir -eq '') { continue }
        if ($File.StartsWith($m.Dir + '/') -and (-not $best -or $m.Dir.Length -gt $members[$best].Dir.Length)) {
            $best = $m.Name
        }
    }
    return $best
}

# ---- classify changed files ----------------------------------------------------------------
$changedCrates = @{}     # crate -> files
$areaWhy = @{}           # area -> files
$unknown = @()
$codeCount = 0
$rootWide = $false
$appUses = @(Get-Dependencies 'forge-app')

foreach ($f in $changed) {
    if ($f -eq 'Cargo.toml' -or $f -eq 'Cargo.lock' -or $f -like '.cargo/*' -or $f -like 'rust-toolchain*') {
        $rootWide = $true; $codeCount++; continue
    }
    if (Test-NonCode $f) { continue }
    $codeCount++
    $owner = Find-Owner $f
    if ($owner) { Add-Item $changedCrates $owner $f }

    $matched = $false
    foreach ($row in $E2EMap) {
        if ($f -like $row[0]) {
            $matched = $true
            foreach ($a in (Expand-Areas $row[1])) { Add-Item $areaWhy $a $f }
        }
    }
    if (-not $matched) {
        # Unmapped code in forge-app or in a crate forge-app uses, or a file outside every crate.
        if (-not $owner -or $appUses -contains $owner) {
            foreach ($a in $AllAreas) { Add-Item $areaWhy $a $f }
            $unknown += $f
        }
    }
}

if ($rootWide) {
    foreach ($n in $members.Keys) { Add-Item $changedCrates $n 'workspace manifest' }
    foreach ($a in $AllAreas) { Add-Item $areaWhy $a 'workspace manifest' }
}

# ---- unit-test plan ------------------------------------------------------------------------
$unitPlan = @()
if (-not $All) {
    $seeds = @($changedCrates.Keys)
    foreach ($c in (Get-Dependents $seeds | Sort-Object)) {
        $m = $members[$c]
        if (-not ($m.Lib -or $m.Bin -or $m.Tests.Count -gt 0)) { continue }
        $cmd = @('test', '-p', $c, '--no-fail-fast')
        if ($m.Lib) { $cmd += '--lib' }
        if ($m.Bin) { $cmd += '--bins' }
        foreach ($t in $m.Tests) { $cmd += @('--test', $t) }
        if ($changedCrates.ContainsKey($c)) {
            $why = 'changed: ' + ((@($changedCrates[$c]) | Select-Object -First 2) -join ', ')
        } else {
            $uses = @(Get-Dependencies $c | Where-Object { $changedCrates.ContainsKey($_) } | Sort-Object)
            $why = 'uses ' + ($uses -join ', ')
        }
        $unitPlan += [pscustomobject]@{ Crate = $c; Cmd = $cmd; Why = $why }
    }
}

# ---- e2e plan ------------------------------------------------------------------------------
if ($All) {
    $e2eAreas = $AllAreas
} else {
    $e2eAreas = @($AllAreas | Where-Object { $areaWhy.ContainsKey($_) })
}

# ---- print the plan ------------------------------------------------------------------------
Write-Host '== test-affected plan =='
Write-Host ("base     {0} (merge-base {1})" -f $Base, $mergeBase.Substring(0, [Math]::Min(8, $mergeBase.Length)))
Write-Host ("changed  {0} file(s), {1} affect code" -f $changed.Count, $codeCount)
if ($All) {
    Write-Host 'mode     -All: cargo test --workspace (unit tests and every e2e area)'
} else {
    if ($unitPlan.Count -eq 0) { Write-Host 'unit     none' }
    foreach ($u in $unitPlan) { Write-Host ("unit     {0,-14} {1}" -f $u.Crate, $u.Why) }
    if ($e2eAreas.Count -eq 0) {
        Write-Host 'e2e      skipped (no app or engine files changed)'
    } else {
        Write-Host ("e2e      {0}" -f (($e2eAreas | ForEach-Object { "$_::" }) -join ' '))
        foreach ($a in $e2eAreas) {
            $files = @($areaWhy[$a])
            $shown = @($files | Select-Object -First 3)
            $more = $files.Count - $shown.Count
            $tail = if ($more -gt 0) { " (+$more more)" } else { '' }
            Write-Host ("         {0,-10} <- {1}{2}" -f "$a::", ($shown -join ', '), $tail)
        }
    }
}
if ($unknown.Count -gt 0) {
    Write-Host ("warning  unmapped file(s), running all e2e areas: {0}" -f ($unknown -join ', '))
}

# ---- build the step list -------------------------------------------------------------------
$steps = @()
if ($All) {
    $steps += [pscustomobject]@{ Name = 'workspace'; Cmd = @('test', '--workspace', '--no-fail-fast') }
} else {
    foreach ($u in $unitPlan) { $steps += [pscustomobject]@{ Name = "unit $($u.Crate)"; Cmd = $u.Cmd } }
    if ($e2eAreas.Count -gt 0) {
        $filters = @($e2eAreas | ForEach-Object { "$_::" })
        $steps += [pscustomobject]@{
            Name = 'e2e'
            Cmd  = @('test', '-p', 'forge-app', '--test', 'e2e', '--no-fail-fast', '--') + $filters
        }
    }
}

if ($steps.Count -eq 0) {
    Write-Host 'nothing to run'
    exit 0
}

if ($DryRun) {
    Write-Host 'dry run, commands that would run:'
    foreach ($s in $steps) { Write-Host ("  cargo {0}" -f ($s.Cmd -join ' ')) }
    exit 0
}

# Smaller debug info keeps target/ from filling the disk. An explicit setting is honoured.
if (-not $env:CARGO_PROFILE_DEV_DEBUG) { $env:CARGO_PROFILE_DEV_DEBUG = '0' }

# ---- run -----------------------------------------------------------------------------------
$total = [Diagnostics.Stopwatch]::StartNew()
$results = @()
foreach ($s in $steps) {
    Write-Host ''
    Write-Host ("== {0}: cargo {1}" -f $s.Name, ($s.Cmd -join ' '))
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & cargo @($s.Cmd)
    $code = $LASTEXITCODE
    $sw.Stop()
    $results += [pscustomobject]@{ Name = $s.Name; Ok = ($code -eq 0); Secs = [Math]::Round($sw.Elapsed.TotalSeconds, 1) }
}
$total.Stop()

Write-Host ''
Write-Host '== summary =='
foreach ($r in $results) {
    $tag = if ($r.Ok) { 'PASS' } else { 'FAIL' }
    Write-Host ("  {0,-4} {1,-24} {2,8:N1} s" -f $tag, $r.Name, $r.Secs)
}
Write-Host ("  total {0:N1} s" -f $total.Elapsed.TotalSeconds)

if ($results | Where-Object { -not $_.Ok }) { exit 1 }
exit 0

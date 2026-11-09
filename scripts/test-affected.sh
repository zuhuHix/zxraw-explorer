#!/usr/bin/env bash
# Runs the tests affected by the current changes: unit tests for the changed crates and the
# crates that depend on them, plus the e2e areas that the changed app files reach.
# Bash port of scripts/test-affected.ps1. Keep the two in step: same E2E map, same logic.
# Needs bash 5.0 or newer (EPOCHREALTIME, nameref).
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/test-affected.sh [--base <ref>] [--all] [--dry-run] [-h|--help]

Runs the tests affected by the current changes: unit tests for the changed crates and the
crates that depend on them, plus the e2e areas that the changed app files reach.

Changes are the files that differ from the merge-base of HEAD and --base (default origin/main),
plus uncommitted and untracked files. Unit tests (lib, bin and integration tests except e2e)
run for every crate with a changed file and every crate that depends on it, directly or not
(from `cargo metadata`). E2E areas come from the E2E map in this script. E2E is skipped when
only docs, *.md, assets, packaging or scripts changed. The plan is printed before anything runs.

Exit code: 0 when everything ran and passed (or nothing was affected), 1 when a step failed.

Options:
  --base <ref>   Ref to compare against (default origin/main). Example: --base HEAD~3
  --all          Run the whole workspace: cargo test --workspace --no-fail-fast (includes e2e).
  --dry-run      Print the plan and the cargo commands, run nothing.
  -h, --help     Show this help.

Example:
  scripts/test-affected.sh --dry-run
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

BASE='origin/main'
ALL=0
DRY_RUN=0
while (($#)); do
  case $1 in
    --base)
      (($# >= 2)) || die '--base needs a ref'
      BASE=$2
      shift 2
      ;;
    --base=*) BASE=${1#--base=}; shift ;;
    --all) ALL=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *)
      printf 'unknown argument: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------------------
# E2E map: changed file -> e2e areas (e2e modules in crates/forge-app/tests/e2e).
# Entries are "glob|areas". Globs are bash [[ == ]] patterns, matched case-insensitively like
# PowerShell -like. Every matching row adds its areas. 'all' means every area. A forge-app
# file that matches no row runs every area.
# Areas: browser, core, ingest, library, media, perf, related, sidebar, system, tools, viewer.
#
# Shared core (app shell, settings, commands, theme, test hook): core only, except where a
# row names the area that uses the file (settings_ui.rs: media-link rules are in related;
# testhook/<area>.rs belongs to its area).
# forge-core and forge-jobs reach every feature, so they run all areas. forge-db, forge-media
# and forge-win run the areas whose features use them (a judgement call; widen a row when in
# doubt).
# ---------------------------------------------------------------------------------------
E2E_MAP=(
  # shared core
  'crates/forge-app/src/app.rs|core'
  'crates/forge-app/src/main.rs|core'
  'crates/forge-app/src/commands.rs|core'
  'crates/forge-app/src/theme.rs|core'
  'crates/forge-app/src/settings.rs|core'
  'crates/forge-app/src/settings_ui.rs|core,related'
  'crates/forge-app/src/actions.rs|core'
  'crates/forge-app/src/cut_watch.rs|core'
  'crates/forge-app/src/clip_worker.rs|core'
  'crates/forge-app/src/modal.rs|core'
  'crates/forge-app/src/palette.rs|core'
  'crates/forge-app/src/dialogs.rs|core'
  'crates/forge-app/src/testhook_off.rs|core'
  'crates/forge-app/src/testhook/*|core'
  'crates/forge-app/src/testhook/browser.rs|browser'
  'crates/forge-app/src/testhook/ingest.rs|ingest'
  'crates/forge-app/src/testhook/library.rs|library'
  'crates/forge-app/src/testhook/media.rs|media'
  'crates/forge-app/src/testhook/perf.rs|perf'
  'crates/forge-app/src/testhook/related.rs|related'
  'crates/forge-app/src/testhook/sidebar.rs|sidebar'
  'crates/forge-app/src/testhook/system.rs|system'
  'crates/forge-app/src/testhook/tools.rs|tools'
  'crates/forge-app/src/testhook/viewer.rs|viewer'
  # library, search, collections
  'crates/forge-app/src/collections_ui.rs|library'
  'crates/forge-app/src/library.rs|library'
  'crates/forge-app/src/search_ui.rs|library'
  'crates/forge-app/src/tags_ui.rs|library'
  'crates/forge-app/src/meta_edit.rs|library,media'
  # viewer
  'crates/forge-app/src/viewer/*|viewer'
  'crates/forge-app/src/viewer_host.rs|viewer'
  # ingest
  'crates/forge-app/src/ingest_ui.rs|ingest'
  # media and related
  'crates/forge-app/src/video_ui.rs|media,related'
  'crates/forge-app/src/media_meta.rs|media'
  'crates/forge-app/src/media_durations.rs|media'
  'crates/forge-app/src/gps_ui.rs|media'
  'crates/forge-app/src/meta_export.rs|media'
  'crates/forge-app/src/export_assoc.rs|related'
  # tools
  'crates/forge-app/src/tools_ui.rs|tools'
  'crates/forge-app/src/extra_tools_ui.rs|tools'
  'crates/forge-app/src/batch_export.rs|tools'
  'crates/forge-app/src/batch_rename.rs|tools,core'
  'crates/forge-app/src/ops_ui.rs|core,tools'
  'crates/forge-app/src/watcher.rs|core,tools'
  # browser (panes, views, sidebar, grouping)
  'crates/forge-app/src/views.rs|browser,perf'
  'crates/forge-app/src/pane.rs|browser,core'
  'crates/forge-app/src/sidebar*.rs|browser'
  'crates/forge-app/src/breadcrumb.rs|browser'
  # sidebar: Quick access pins and saved layouts
  'crates/forge-app/src/sidebar.rs|sidebar'
  'crates/forge-app/src/sidebar/*|sidebar'
  'crates/forge-app/src/layouts.rs|sidebar'
  'crates/forge-app/src/path_probe.rs|sidebar'
  'crates/forge-app/src/similar.rs|browser'
  'crates/forge-app/src/platform.rs|browser'
  # thumbnails and previews are used by the grid everywhere
  'crates/forge-app/src/thumbs.rs|core,viewer,perf'
  'crates/forge-app/src/preview.rs|core,viewer,perf'
  # core-only leaf modules
  'crates/forge-app/src/dragout.rs|core'
  'crates/forge-app/src/crash.rs|core'
  'crates/forge-app/src/notices.rs|core'
  'crates/forge-app/src/fonts.rs|core'
  'crates/forge-app/src/app_icon.rs|core'
  'crates/forge-app/src/default_app.rs|core,system'
  # system: updater and ffmpeg download
  'crates/forge-app/src/updater.rs|system'
  'crates/forge-app/src/ffmpeg_setup.rs|system'
  # e2e harness and area modules
  'crates/forge-app/tests/e2e/main.rs|all'
  'crates/forge-app/tests/e2e/browser.rs|browser'
  'crates/forge-app/tests/e2e/core.rs|core'
  'crates/forge-app/tests/e2e/ingest.rs|ingest'
  'crates/forge-app/tests/e2e/library.rs|library'
  'crates/forge-app/tests/e2e/media.rs|media'
  'crates/forge-app/tests/e2e/perf.rs|perf'
  'crates/forge-app/tests/e2e/related.rs|related'
  'crates/forge-app/tests/e2e/sidebar.rs|sidebar'
  'crates/forge-app/tests/e2e/system.rs|system'
  'crates/forge-app/tests/e2e/tools.rs|tools'
  'crates/forge-app/tests/e2e/viewer.rs|viewer'
  # crates that forge-app links
  'crates/forge-core/*|all'
  'crates/forge-jobs/*|all'
  'crates/forge-db/*|core,library,viewer,ingest,related'
  'crates/forge-media/*|core,media,viewer,related,system'
  'crates/forge-win/*|core,library,system,tools'
)

ALL_AREAS=(browser core ingest library media perf related sidebar system tools viewer)

# cargo metadata -> one line per workspace package: name|dir|lib|bin|tests|path-deps
# (lib/bin are 1 or 0; tests and deps are space-separated). Same rules as the .ps1.
JQ_FILTER='.packages[]
  | (.manifest_path | sub("/[^/]+$"; "")) as $dir
  | select($dir | startswith($root))
  | [.targets[].kind[]] as $kinds
  | "\(.name)|\($dir[($root | length):] | sub("^/+"; ""))|\(if any($kinds[]; . == "lib" or . == "rlib" or . == "proc-macro") then 1 else 0 end)|\(if any($kinds[]; . == "bin") then 1 else 0 end)|\([.targets[] | select(any(.kind[]; . == "test") and .name != "e2e") | .name] | join(" "))|\([.dependencies[] | select(.path) | .name] | join(" "))"'

PY_FILTER='
import json, sys
root = sys.argv[1]
for p in json.load(sys.stdin)["packages"]:
    d = p["manifest_path"][: p["manifest_path"].rfind("/")]
    if not d.startswith(root):
        continue
    kinds = [k for t in p["targets"] for k in t["kind"]]
    tests = [t["name"] for t in p["targets"] if "test" in t["kind"] and t["name"] != "e2e"]
    deps = [x["name"] for x in p["dependencies"] if x.get("path")]
    print("|".join([
        p["name"],
        d[len(root):].lstrip("/"),
        "1" if any(k in ("lib", "rlib", "proc-macro") for k in kinds) else "0",
        "1" if "bin" in kinds else "0",
        " ".join(tests),
        " ".join(deps),
    ]))
'

# PowerShell -like is case-insensitive; bash [[ == ]] is not unless nocasematch is on.
ps_like() { # ps_like <path> <glob>
  local matched=1
  shopt -s nocasematch
  [[ $1 == $2 ]] && matched=0
  shopt -u nocasematch
  return "$matched"
}

list_has() { # list_has <needle> <newline-separated list>
  case $'\n'"$2"$'\n' in
    *$'\n'"$1"$'\n'*) return 0 ;;
  esac
  return 1
}

add_item() { # add_item <assoc-array-name> <key> <value>: append value to key's list once
  local -n _tbl=$1
  local _cur=${_tbl["$2"]-}
  if ! list_has "$3" "$_cur"; then
    if [[ -n $_cur ]]; then
      _tbl["$2"]=$_cur$'\n'$3
    else
      _tbl["$2"]=$3
    fi
  fi
}

join_by() { # join_by <sep> <items...>
  local sep=$1 out='' first=1 x
  shift
  for x in "$@"; do
    if ((first)); then
      out=$x
      first=0
    else
      out+=$sep$x
    fi
  done
  printf '%s' "$out"
}

expand_areas() { # expand_areas <spec>: sets AREAS to the areas a map spec names
  AREAS=()
  if [[ $1 == all ]]; then
    AREAS=("${ALL_AREAS[@]}")
    return
  fi
  local part IFS=,
  for part in $1; do
    AREAS+=("${part// /}")
  done
}

is_non_code() { # PowerShell Test-NonCode
  ps_like "$1" 'docs/*' || ps_like "$1" 'assets/*' || ps_like "$1" 'packaging/*' ||
    ps_like "$1" 'scripts/*' || ps_like "$1" '*.md' || ps_like "$1" 'LICENSE*' ||
    ps_like "$1" '.gitignore'
}

now_us() { # microseconds since the epoch, as an integer
  # EPOCHREALTIME uses the locale's decimal mark (a comma in some locales), so keep digits only.
  local t=$EPOCHREALTIME
  printf '%s' "${t//[!0-9]/}"
}

fmt_secs() { # fmt_secs <start-us> <end-us>: elapsed seconds with one decimal
  local t=$(((${2} - ${1} + 50000) / 100000))
  printf '%d.%d' $((t / 10)) $((t % 10))
}

# Crates that depend (directly or not) on the seeds, seeds included. One per line.
closure_up() { # closure_up <seed>...
  local -A seen=()
  local queue=("$@") i=0 c d
  local ds=()
  for c in "$@"; do seen["$c"]=1; done
  while ((i < ${#queue[@]})); do
    c=${queue[i]}
    i=$((i + 1))
    read -ra ds <<< "${DEPENDENTS["$c"]-}"
    for d in "${ds[@]}"; do
      if [[ -z ${seen["$d"]+x} ]]; then
        seen["$d"]=1
        queue+=("$d")
      fi
    done
  done
  for c in "${!seen[@]}"; do printf '%s\n' "$c"; done
}

# The seed and every workspace crate it uses (directly or not). One per line.
closure_down() { # closure_down <seed>
  local seed=$1
  local -A seen=(["$seed"]=1)
  local queue=("$seed") i=0 c d
  local ds=()
  while ((i < ${#queue[@]})); do
    c=${queue[i]}
    i=$((i + 1))
    read -ra ds <<< "${M_DEPS["$c"]-}"
    for d in "${ds[@]}"; do
      if [[ -n ${M_DIR["$d"]+x} && -z ${seen["$d"]+x} ]]; then
        seen["$d"]=1
        queue+=("$d")
      fi
    done
  done
  for c in "${!seen[@]}"; do printf '%s\n' "$c"; done
}

find_owner() { # find_owner <file>: the crate whose directory holds the file (longest match)
  local f=$1 best='' best_len=0 m dir
  for m in "${MEMBERS[@]}"; do
    dir=${M_DIR["$m"]}
    [[ -n $dir ]] || continue
    if [[ $f == "$dir"/* ]] && ((${#dir} > best_len)); then
      best=$m
      best_len=${#dir}
    fi
  done
  printf '%s' "$best"
}

# ---- repo, base and changed files ----------------------------------------------------------
root=$(git rev-parse --show-toplevel) || die 'not inside a git repository'
cd "$root"

merge_base=$(git merge-base HEAD "$BASE") || die "git merge-base HEAD $BASE failed"
merge_base=${merge_base%%$'\n'*}
tracked=$(git diff --name-only "$merge_base") || die "git diff --name-only $merge_base failed"
untracked=$(git ls-files --others --exclude-standard) || die 'git ls-files --others failed'
mapfile -t changed < <(printf '%s\n' "$tracked" "$untracked" | grep -v '^$' | LC_ALL=C sort -u)

# ---- workspace crates from cargo metadata -------------------------------------------------
meta=$(cargo metadata --format-version 1 --no-deps) || die 'cargo metadata failed'
if command -v jq >/dev/null 2>&1; then
  members_out=$(printf '%s' "$meta" | jq -r --arg root "$root" "$JQ_FILTER")
else
  members_out=$(printf '%s' "$meta" | python3 -I -c "$PY_FILTER" "$root")
fi

declare -A M_DIR=() M_LIB=() M_BIN=() M_TESTS=() M_DEPS=() DEPENDENTS=()
MEMBERS=()
while IFS='|' read -r name dir lib bin tests deps; do
  [[ -n $name ]] || continue
  MEMBERS+=("$name")
  M_DIR["$name"]=$dir
  M_LIB["$name"]=$lib
  M_BIN["$name"]=$bin
  M_TESTS["$name"]=$tests
  M_DEPS["$name"]=$deps
done <<< "$members_out"

for name in "${MEMBERS[@]}"; do
  read -ra ds <<< "${M_DEPS["$name"]-}"
  for d in "${ds[@]}"; do
    if [[ -n ${M_DIR["$d"]+x} ]]; then
      add_item DEPENDENTS "$d" "$name"
    fi
  done
done

# ---- classify changed files ----------------------------------------------------------------
declare -A CHANGED_CRATES=() AREA_WHY=()
unknown=()
code_count=0
root_wide=0
app_uses=$(closure_down forge-app)

for f in "${changed[@]}"; do
  if ps_like "$f" 'Cargo.toml' || ps_like "$f" 'Cargo.lock' || ps_like "$f" '.cargo/*' ||
    ps_like "$f" 'rust-toolchain*'; then
    root_wide=1
    code_count=$((code_count + 1))
    continue
  fi
  if is_non_code "$f"; then
    continue
  fi
  code_count=$((code_count + 1))
  owner=$(find_owner "$f")
  if [[ -n $owner ]]; then
    add_item CHANGED_CRATES "$owner" "$f"
  fi

  matched=0
  for row in "${E2E_MAP[@]}"; do
    glob=${row%%|*}
    spec=${row#*|}
    if ps_like "$f" "$glob"; then
      matched=1
      expand_areas "$spec"
      for a in "${AREAS[@]}"; do
        add_item AREA_WHY "$a" "$f"
      done
    fi
  done
  if ((!matched)); then
    # Unmapped code in forge-app or in a crate forge-app uses, or a file outside every crate.
    if [[ -z $owner ]] || list_has "$owner" "$app_uses"; then
      for a in "${ALL_AREAS[@]}"; do
        add_item AREA_WHY "$a" "$f"
      done
      unknown+=("$f")
    fi
  fi
done

if ((root_wide)); then
  for name in "${MEMBERS[@]}"; do
    add_item CHANGED_CRATES "$name" 'workspace manifest'
  done
  for a in "${ALL_AREAS[@]}"; do
    add_item AREA_WHY "$a" 'workspace manifest'
  done
fi

# ---- unit-test plan ------------------------------------------------------------------------
unit_names=()
unit_cmds=()
unit_whys=()
if ((!ALL)); then
  seeds=("${!CHANGED_CRATES[@]}")
  mapfile -t up < <(closure_up "${seeds[@]}" | LC_ALL=C sort)
  for c in "${up[@]}"; do
    [[ -n $c ]] || continue
    lib=${M_LIB["$c"]}
    bin=${M_BIN["$c"]}
    tests=${M_TESTS["$c"]}
    if [[ $lib != 1 && $bin != 1 && -z $tests ]]; then
      continue
    fi
    cmd="test -p $c --no-fail-fast"
    if [[ $lib == 1 ]]; then cmd+=' --lib'; fi
    if [[ $bin == 1 ]]; then cmd+=' --bins'; fi
    read -ra test_names <<< "$tests"
    for t in "${test_names[@]}"; do
      cmd+=" --test $t"
    done
    if [[ -n ${CHANGED_CRATES["$c"]+x} ]]; then
      mapfile -t files <<< "${CHANGED_CRATES["$c"]}"
      why="changed: $(join_by ', ' "${files[@]:0:2}")"
    else
      mapfile -t used < <(closure_down "$c" | LC_ALL=C sort)
      uses=()
      for d in "${used[@]}"; do
        if [[ -n ${CHANGED_CRATES["$d"]+x} ]]; then
          uses+=("$d")
        fi
      done
      why="uses $(join_by ', ' "${uses[@]}")"
    fi
    unit_names+=("$c")
    unit_cmds+=("$cmd")
    unit_whys+=("$why")
  done
fi

# ---- e2e plan ------------------------------------------------------------------------------
E2E_AREAS=()
if ((ALL)); then
  E2E_AREAS=("${ALL_AREAS[@]}")
else
  for a in "${ALL_AREAS[@]}"; do
    if [[ -n ${AREA_WHY["$a"]+x} ]]; then
      E2E_AREAS+=("$a")
    fi
  done
fi

# ---- print the plan ------------------------------------------------------------------------
echo '== test-affected plan =='
printf 'base     %s (merge-base %s)\n' "$BASE" "${merge_base:0:8}"
printf 'changed  %d file(s), %d affect code\n' "${#changed[@]}" "$code_count"
if ((ALL)); then
  echo 'mode     -All: cargo test --workspace (unit tests and every e2e area)'
else
  if ((${#unit_names[@]} == 0)); then
    echo 'unit     none'
  fi
  for i in "${!unit_names[@]}"; do
    printf 'unit     %-14s %s\n' "${unit_names[i]}" "${unit_whys[i]}"
  done
  if ((${#E2E_AREAS[@]} == 0)); then
    echo 'e2e      skipped (no app or engine files changed)'
  else
    tags=()
    for a in "${E2E_AREAS[@]}"; do
      tags+=("$a::")
    done
    printf 'e2e      %s\n' "$(join_by ' ' "${tags[@]}")"
    for a in "${E2E_AREAS[@]}"; do
      mapfile -t files <<< "${AREA_WHY["$a"]}"
      shown=("${files[@]:0:3}")
      more=$((${#files[@]} - ${#shown[@]}))
      tail=''
      if ((more > 0)); then
        tail=" (+$more more)"
      fi
      printf '         %-10s <- %s%s\n' "$a::" "$(join_by ', ' "${shown[@]}")" "$tail"
    done
  fi
fi
if ((${#unknown[@]} > 0)); then
  printf 'warning  unmapped file(s), running all e2e areas: %s\n' "$(join_by ' ' "${unknown[@]}")"
fi

# ---- build the step list -------------------------------------------------------------------
step_names=()
step_cmds=()
if ((ALL)); then
  step_names+=(workspace)
  step_cmds+=('test --workspace --no-fail-fast')
else
  for i in "${!unit_names[@]}"; do
    step_names+=("unit ${unit_names[i]}")
    step_cmds+=("${unit_cmds[i]}")
  done
  if ((${#E2E_AREAS[@]} > 0)); then
    filters=()
    for a in "${E2E_AREAS[@]}"; do
      filters+=("$a::")
    done
    step_names+=(e2e)
    step_cmds+=("test -p forge-app --test e2e --no-fail-fast -- $(join_by ' ' "${filters[@]}")")
  fi
fi

if ((${#step_names[@]} == 0)); then
  echo 'nothing to run'
  exit 0
fi

if ((DRY_RUN)); then
  echo 'dry run, commands that would run:'
  for c in "${step_cmds[@]}"; do
    echo "  cargo $c"
  done
  exit 0
fi

# Smaller debug info keeps target/ from filling the disk. An explicit setting is honoured.
: "${CARGO_PROFILE_DEV_DEBUG:=0}"
export CARGO_PROFILE_DEV_DEBUG

# ---- run -----------------------------------------------------------------------------------
total_start=$(now_us)
result_codes=()
result_secs=()
for i in "${!step_names[@]}"; do
  echo ''
  echo "== ${step_names[i]}: cargo ${step_cmds[i]}"
  read -ra args <<< "${step_cmds[i]}"
  s0=$(now_us)
  code=0
  cargo "${args[@]}" || code=$?
  s1=$(now_us)
  result_codes+=("$code")
  result_secs+=("$(fmt_secs "$s0" "$s1")")
done
total_end=$(now_us)

echo ''
echo '== summary =='
failed=0
for i in "${!step_names[@]}"; do
  tag=PASS
  if ((result_codes[i] != 0)); then
    tag=FAIL
    failed=1
  fi
  printf '  %-4s %-24s %8s s\n' "$tag" "${step_names[i]}" "${result_secs[i]}"
done
printf '  total %s s\n' "$(fmt_secs "$total_start" "$total_end")"

exit "$failed"

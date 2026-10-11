"""Release-build frame-time benchmark for a 50,000-file folder.

Runs the same script as crates/forge-app/tests/e2e/perf.rs against a release `zxraw.exe`
built with `--features automation`, N times with a fresh profile each, and prints the
per-run JSON reports plus the median of each statistic across runs.

Usage: python -I bench50k.py <zxraw.exe> <work dir> [runs]
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

FILES = 50_000
EXTS = ["JPG", "CR3", "MP4", "XMP", "TXT"]


def make_folder(folder):
    if os.path.isdir(folder) and len(os.listdir(folder)) == FILES:
        return
    os.makedirs(folder, exist_ok=True)
    for i in range(FILES):
        open(os.path.join(folder, f"IMG_{i:05}.{EXTS[i % len(EXTS)]}"), "wb").close()


def script(folder, out, run):
    scroll = lambda n, d: "".join(f"scroll {d}\n" for _ in range(n))
    j = lambda name: os.path.join(out, f"run{run}-{name}.json")
    return (
        "limit 300\n"
        f"navigate {folder}\n"
        f"wait-entry IMG_{FILES - 1:05}.TXT | 120\n"
        "cmd ViewDetails\nwait-frames 10\npointer 700 420\nui perf begin\n"
        + scroll(150, -240) + scroll(150, 240)
        + f"ui perf end {j('details')}\n"
        "cmd ViewGrid\nwait-frames 10\npointer 700 420\nui perf begin\n"
        + scroll(150, -240) + scroll(150, 240)
        + f"ui perf end {j('grid')}\n"
        "cmd ViewDetails\nwait-frames 10\nselect IMG_00000.JPG\nui perf begin\n"
        + "key PageDown\n" * 60
        + "key End\nwait-frames 5\nkey Home\nwait-frames 5\n"
        + f"ui perf end {j('keys')}\n"
        "exit\n"
    )


def main():
    exe, work = sys.argv[1], sys.argv[2]
    runs = int(sys.argv[3]) if len(sys.argv) > 3 else 5
    folder = os.path.join(work, "flat50k")
    out = os.path.join(work, "reports")
    os.makedirs(out, exist_ok=True)
    make_folder(folder)
    for run in range(runs):
        profile = os.path.join(work, f"profile-{run}")
        shutil.rmtree(profile, ignore_errors=True)
        steps = os.path.join(work, f"steps-{run}.txt")
        result = os.path.join(work, f"steps-{run}.result")
        with open(steps, "w", encoding="utf-8") as f:
            f.write(script(folder, out, run))
        env = dict(os.environ)
        env.update({
            "ZXRAW_SCRIPT": steps,
            "ZXRAW_SCRIPT_RESULT": result,
            "ZXRAW_SCRIPT_SANDBOX": work,
            "ZXRAW_PROFILE_DIR": profile,
        })
        started = time.time()
        proc = subprocess.run([exe], env=env, timeout=400)
        outcome = open(result, encoding="utf-8").read().strip() if os.path.exists(result) else "no result"
        print(f"run {run}: exit {proc.returncode}, {outcome}, {time.time() - started:.0f} s", flush=True)
    stats = {}
    for name in ["details", "grid", "keys"]:
        for series in ["work_ms", "interval_ms"]:
            for key in ["p50", "p95", "p99", "max", "mean"]:
                vals = []
                for run in range(runs):
                    p = os.path.join(out, f"run{run}-{name}.json")
                    if os.path.exists(p):
                        r = json.load(open(p, encoding="utf-8"))
                        if r.get(series):
                            vals.append(r[series][key])
                if vals:
                    stats[f"{name}.{series}.{key}"] = (statistics.median(vals), min(vals), max(vals), len(vals))
    for k, (med, lo, hi, n) in stats.items():
        print(f"{k:28} median {med:8.2f}  range {lo:8.2f}-{hi:8.2f}  (n={n})")


if __name__ == "__main__":
    main()

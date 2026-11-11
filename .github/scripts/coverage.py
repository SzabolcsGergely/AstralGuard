"""Line coverage per file from `cargo llvm-cov --json`, checked against per-file floors.

    python3 .github/scripts/coverage.py coverage.json .github/coverage-floors.txt

Writes a table to $GITHUB_STEP_SUMMARY (or stdout). Only the files in the floors file are
enforced: the code that makes decisions. Hardware, FFI and GUI code cannot run in CI and is
reported only. A floor is raised when coverage rises, never lowered to let a change through.
"""

import json
import os
import sys


def load_floors(path):
    floors = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if line:
                name, pct = line.split()
                floors[name] = float(pct)
    return floors


def main():
    report, floors_path = sys.argv[1], sys.argv[2]
    data = json.load(open(report, encoding="utf-8"))["data"][0]
    floors = load_floors(floors_path)
    rows = {}
    for f in data["files"]:
        name = f["filename"].replace("\\", "/")
        if "/rust/src/" not in name:
            continue
        name = "src/" + name.split("/rust/src/", 1)[1]
        lines = f["summary"]["lines"]
        rows[name] = (lines["covered"], lines["count"], lines["percent"])

    failures = []
    out = ["## Line coverage", "", "| File | Lines | Covered | Floor |", "|---|---:|---:|---:|"]
    for name, floor in sorted(floors.items()):
        if name not in rows:
            failures.append(f"{name}: in the floors file but not in the report (renamed?)")
            continue
        covered, count, pct = rows[name]
        mark = ""
        if pct + 1e-9 < floor:
            failures.append(f"{name}: {pct:.1f}% is below its floor of {floor:.1f}%")
            mark = " (below)"
        out.append(f"| **{name}** | {count} | {pct:.1f}%{mark} | {floor:.1f}% |")
    for name, (covered, count, pct) in sorted(rows.items()):
        if name not in floors:
            out.append(f"| {name} | {count} | {pct:.1f}% | |")
    total = data["totals"]["lines"]
    out += ["", f"Total: {total['percent']:.1f}% of {total['count']} lines (reported, not enforced)."]
    if failures:
        out += ["", "**Below the floor:**", ""] + [f"- {x}" for x in failures]

    text = "\n".join(out) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    print(text)
    for x in failures:
        print(f"::error::coverage: {x}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()

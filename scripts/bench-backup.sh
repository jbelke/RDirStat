#!/usr/bin/env bash
# Backup throughput harness (.settings/docs/12-THROUGHPUT.md, slice 0).
#
#   scripts/bench-backup.sh <corpus-dir> <dest-dir> [report.json]
#
# Builds rdirstat-cli with the `phase-timing` feature (release), runs one
# backup job of <corpus-dir> into <dest-dir> through the service's own runner
# (plan, apply, journal, catalog, index), prints wall time per phase
# (seconds, % of wall, MiB/s) and writes the full report as JSON.
#
# Make the corpus with scripts/gen-bench-corpus.py. <dest-dir> must be empty
# or absent: a second run into the same dest finds everything already landed
# and measures nothing. The primary catalog goes to a fresh dir under $TMPDIR,
# as a real job's goes to the boot disk.
#
# <corpus-dir> and <dest-dir> must be on different volumes: on one APFS
# volume `ditto` clones instead of copying, and the ditto and compare times
# mean nothing. For an SSD-to-SSD run on one disk, put the dest on a sparse
# disk image (hdiutil create -fs APFS -type SPARSEBUNDLE; hdiutil attach).
# The harness refuses a same-volume pair.
#
# MiB/s is payload bytes per second of the phase: a compare reads each byte
# twice (source and copy), so its disk traffic is twice its MiB/s. The page
# cache is not purged (that needs root): a corpus written just before the run
# is read from RAM, so run `sudo purge` first for cold source reads.
#
# Exit codes: 0 the job landed everything; 3 it ended incomplete (the report
# is still written); 1 the harness itself failed.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <corpus-dir> <dest-dir> [report.json]" >&2
  exit 1
fi
corpus=$1
dest=$2
stamp=$(date +%Y%m%d-%H%M%S)
report=${3:-$PWD/bench-backup-$stamp.json}
repo=$(cd "$(dirname "$0")/.." && pwd)

[[ -d $corpus ]] || { echo "corpus $corpus is not a directory" >&2; exit 1; }
mkdir -p "$dest"
if [[ -n $(ls -A "$dest") ]]; then
  echo "dest $dest is not empty; use a fresh one" >&2
  exit 1
fi

if [[ $(stat -f %d "$corpus") == $(stat -f %d "$dest") ]]; then
  echo "corpus and dest are on one volume: ditto would clone, not copy" >&2
  exit 1
fi

(cd "$repo" && cargo build --release --quiet -p rdirstat-cli --features phase-timing)
bin=$repo/target/release/rdirstat-cli
catalog=$(mktemp -d "${TMPDIR:-/tmp}/rdirstat-bench-catalog.XXXXXX")
raw=$(mktemp "${TMPDIR:-/tmp}/rdirstat-bench-raw.XXXXXX")

set +e
"$bin" backup bench --source "$corpus" --dest "$dest" --catalog "$catalog" --out "$raw" >/dev/null
code=$?
set -e
if [[ ! -s $raw ]]; then
  echo "bench produced no report (exit $code)" >&2
  exit 1
fi

python3 - "$raw" "$report" "$corpus" "$dest" <<'PY'
import json, sys
raw, report, corpus, dest = sys.argv[1:5]
r = json.load(open(raw))
wall = r["wall_seconds"]
MIB = 1024 * 1024
rows = []
for p in r["phases"]:
    s = p["seconds"]
    rows.append({
        "phase": p["phase"],
        "seconds": round(s, 3),
        "pct_wall": round(100 * s / wall, 1) if wall else 0.0,
        "mib_s": round(p["bytes"] / MIB / s, 1) if p["bytes"] and s > 0 else None,
        "calls": p["calls"],
        "bytes": p["bytes"],
    })
r["corpus"] = corpus
r["dest"] = dest
r["table"] = rows
r["overall_mib_s"] = round(r["landed_bytes"] / MIB / wall, 1) if wall else None
json.dump(r, open(report, "w"), indent=2)
print(f"{'phase':<14} {'seconds':>10} {'% wall':>7} {'MiB/s':>8} {'calls':>9}")
for row in rows:
    mib = "" if row["mib_s"] is None else f"{row['mib_s']:.1f}"
    print(f"{row['phase']:<14} {row['seconds']:>10.2f} {row['pct_wall']:>6.1f}% {mib:>8} {row['calls']:>9}")
print(f"{'wall':<14} {wall:>10.2f} {100.0:>6.1f}% {r['overall_mib_s'] or 0:>8.1f}")
print(f"landed {r['landed_items']} items, {r['landed_bytes'] / MIB:.0f} MiB; ended {r['ended']}"
      + (f"; last error: {r['last_error']}" if r.get('last_error') else ""))
print(f"report: {report}")
PY
rm -f "$raw"
rm -rf "$catalog"
exit "$code"

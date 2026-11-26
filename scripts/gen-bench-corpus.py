#!/usr/bin/env python3
"""Generate the fixed backup-throughput corpus (.settings/docs/12-THROUGHPUT.md, slice 0).

Default: 20 large files of 1-4 GiB and 200,000 small files of 1-64 KiB in a
realistic directory tree (projects, photos, documents, caches), all from one
seed, so two runs with the same arguments produce the same names, sizes and
bytes. Contents are pseudo-random (incompressible) and no two 1 MiB blocks
are equal, so nothing dedupes or clones.

    scripts/gen-bench-corpus.py <out-dir> [--seed 12] [--scale 1.0] [--max-total-gib 60]

--scale multiplies both file counts and the large-file sizes (0.1 for a quick
run). --max-total-gib shrinks the large files proportionally if the planned
total would exceed it. <out-dir> must not exist or be empty. Prints the
totals as one JSON line.
"""

import argparse
import json
import os
import random
import struct
import sys

KIB = 1024
MIB = 1024 * KIB
GIB = 1024 * MIB
BLOCK = MIB

TOP = {
    "Projects": ["src", "tests", "docs", "assets", "build", "vendor"],
    "Photos": ["2019", "2020", "2021", "2022", "2023", "2024"],
    "Documents": ["Invoices", "Letters", "Taxes", "Manuals", "Notes"],
    "Library/Caches": ["com.apple.Safari", "pip", "npm", "cargo", "go-build"],
    "Music": ["Albums", "Podcasts", "Samples"],
}
LARGE_HOMES = ["Movies", "VMs", "Archives", "Photos/RAW"]
EXT = [".txt", ".md", ".rs", ".js", ".json", ".jpg", ".png", ".pdf", ".o", ".plist", ".cache", ".heic"]


def dirs_for(rng, count):
    """`count` relative directory paths, 2-5 levels deep, under TOP."""
    out = []
    tops = list(TOP)
    while len(out) < count:
        top = rng.choice(tops)
        parts = [top, rng.choice(TOP[top])]
        for _ in range(rng.randint(0, 3)):
            parts.append("%s-%03d" % (rng.choice(["mod", "album", "batch", "pkg", "part", "set"]), rng.randrange(1000)))
        out.append(os.path.join(*parts))
    return sorted(set(out))


class Payload:
    """Pseudo-random bytes from one seeded pool; each block is stamped with a
    unique counter so no two blocks (and no two files) are equal."""

    def __init__(self, seed):
        self.pool = random.Random(seed).randbytes(64 * MIB + BLOCK)
        self.counter = 0

    def block(self, size):
        self.counter += 1
        start = (self.counter * 7919 * 4096) % (64 * MIB)
        data = bytearray(self.pool[start : start + size])
        stamp = struct.pack("<Q", self.counter)
        data[: min(8, size)] = stamp[: min(8, size)]
        return bytes(data)

    def write(self, path, size):
        with open(path, "wb") as f:
            left = size
            while left > 0:
                n = min(BLOCK, left)
                f.write(self.block(n))
                left -= n


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out")
    ap.add_argument("--seed", type=int, default=12)
    ap.add_argument("--scale", type=float, default=1.0)
    ap.add_argument("--large-count", type=int, default=20)
    ap.add_argument("--small-count", type=int, default=200_000)
    ap.add_argument("--max-total-gib", type=float, default=60.0)
    args = ap.parse_args()

    if os.path.exists(args.out) and os.listdir(args.out):
        sys.exit(f"{args.out} exists and is not empty")
    rng = random.Random(args.seed)
    large_n = max(0, round(args.large_count * min(args.scale, 1.0)))
    small_n = max(1, round(args.small_count * args.scale))
    large_sizes = [int(rng.uniform(1, 4) * GIB * min(args.scale, 1.0)) // BLOCK * BLOCK for _ in range(large_n)]
    small_sizes = [rng.randint(1 * KIB, 64 * KIB) for _ in range(small_n)]
    budget = int(args.max_total_gib * GIB) - sum(small_sizes)
    if budget <= 0:
        sys.exit("--max-total-gib is below the small files alone")
    if large_sizes and sum(large_sizes) > budget:
        factor = budget / sum(large_sizes)
        large_sizes = [max(BLOCK, int(s * factor) // BLOCK * BLOCK) for s in large_sizes]

    payload = Payload(args.seed)
    os.makedirs(args.out, exist_ok=True)

    for i, size in enumerate(large_sizes):
        home = os.path.join(args.out, LARGE_HOMES[i % len(LARGE_HOMES)])
        os.makedirs(home, exist_ok=True)
        payload.write(os.path.join(home, "large-%02d%s" % (i, rng.choice([".mov", ".dmg", ".tar", ".vmdk"]))), size)

    # ~20-400 files per directory, like a real tree.
    dirs = dirs_for(rng, max(1, small_n // 120))
    for d in dirs:
        os.makedirs(os.path.join(args.out, d), exist_ok=True)
    for i, size in enumerate(small_sizes):
        d = dirs[min(int(rng.paretovariate(1.2)) - 1, len(dirs) - 1) if rng.random() < 0.3 else rng.randrange(len(dirs))]
        name = "f%06d%s" % (i, rng.choice(EXT))
        with open(os.path.join(args.out, d, name), "wb") as f:
            f.write(payload.block(size))

    print(
        json.dumps(
            {
                "out": os.path.abspath(args.out),
                "seed": args.seed,
                "large_files": len(large_sizes),
                "large_bytes": sum(large_sizes),
                "small_files": len(small_sizes),
                "small_bytes": sum(small_sizes),
                "dirs": len(dirs),
                "total_gib": round((sum(large_sizes) + sum(small_sizes)) / GIB, 2),
            }
        )
    )


if __name__ == "__main__":
    main()

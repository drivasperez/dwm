#!/usr/bin/env python3
"""Compare two release binaries against a real repo, alternating run order.

Runs normal `dwm status`, including its jj working-copy snapshot. Repository
contents should stay still during measurement. Only timings and output hashes
are recorded; workspace descriptions and paths from status are not saved.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    if args.runs < 1 or args.warmups < 0 or args.timeout <= 0:
        parser.error("runs and timeout must be positive; warmups must be nonnegative")
    binaries = [args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)]
    samples = [[], []]
    hashes = [set(), set()]
    env = dict(os.environ, NO_COLOR="1", CLICOLOR="0", CLICOLOR_FORCE="0")
    for iteration in range(args.warmups + args.runs):
        for index in ([0, 1] if iteration % 2 == 0 else [1, 0]):
            start = time.perf_counter()
            result = subprocess.run(
                [str(binaries[index]), "status"],
                cwd=args.repo,
                env=env,
                capture_output=True,
                timeout=args.timeout,
                check=True,
            )
            elapsed = time.perf_counter() - start
            if iteration >= args.warmups:
                samples[index].append(elapsed)
                hashes[index].add(hashlib.sha256(result.stdout + b"\0" + result.stderr).hexdigest())
    report = {}
    for index, label in enumerate(["baseline", "candidate"]):
        report[label] = {
            "seconds": samples[index],
            "median_seconds": statistics.median(samples[index]),
            "min_seconds": min(samples[index]),
            "max_seconds": max(samples[index]),
            "output_sha256": sorted(hashes[index]),
        }
    report["median_speedup"] = report["baseline"]["median_seconds"] / report["candidate"]["median_seconds"]
    # Relative ages and live agent status can legitimately change between runs.
    report["all_outputs_identical"] = len(hashes[0] | hashes[1]) == 1
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()

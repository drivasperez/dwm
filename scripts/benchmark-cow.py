#!/usr/bin/env python3
"""Measure synthetic workspaces on a chosen filesystem; emits JSON.

All repositories and worktrees are owned temporary fixtures. For useful physical
allocation figures use --directory on a quiet, dedicated APFS/Btrfs/XFS volume.
No elevated privileges or mounts are required or created by this script.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--directory", type=Path, default=Path(tempfile.gettempdir()))
    parser.add_argument("--workspaces", type=int, default=5)
    parser.add_argument("--small-files", type=int, default=1000)
    parser.add_argument("--large-mib", type=int, default=4)
    parser.add_argument("--different-revisions", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if min(args.workspaces, args.small_files, args.large_mib) < 1:
        parser.error("fixture sizes must be positive")
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
    results = []

    def run(root, *command):
        return subprocess.run(command, cwd=root, env=env, check=True,
                              capture_output=True, text=True)

    def free(root):
        if hasattr(os, "sync"):
            os.sync()
        return shutil.disk_usage(root).free

    with tempfile.TemporaryDirectory(prefix="dwm-cow-bench-", dir=args.directory) as directory:
        base = Path(directory)
        env["XDG_DATA_HOME"] = str(base / "data")
        env["XDG_CONFIG_HOME"] = str(base / "config")
        env["DWM_REGISTRY_PATH"] = str(base / "repos.txt")
        for seed in (False, True):
            for mode in ("standard", "cow"):
                root = base / f"{mode}-seed-{seed}"
                root.mkdir()
                baseline = free(root)
                run(root, "git", "init", "-q", "-b", "main")
                run(root, "git", "config", "user.name", "dwm benchmark")
                run(root, "git", "config", "user.email", "benchmark@example.invalid")
                run(root, "git", "config", "commit.gpgsign", "false")
                (root / ".gitignore").write_text(".dwm/\n.dwm.toml\ncache/\n")
                (root / "small").mkdir()
                for i in range(args.small_files):
                    (root / "small" / str(i)).write_bytes(os.urandom(4096))
                for i in range(4):
                    (root / f"large-{i}").write_bytes(os.urandom(args.large_mib * 1024 * 1024))
                run(root, "git", "add", ".")
                run(root, "git", "commit", "-qm", "fixture")
                revisions = [run(root, "git", "rev-parse", "HEAD").stdout.strip()]
                if args.different_revisions:
                    for i in range(1, args.workspaces):
                        (root / "small" / "0").write_text(f"revision {i}\n")
                        run(root, "git", "add", ".")
                        run(root, "git", "commit", "-qm", f"revision {i}")
                        revisions.append(run(root, "git", "rev-parse", "HEAD").stdout.strip())
                (root / "cache").mkdir()
                for i in range(4):
                    (root / "cache" / str(i)).write_bytes(os.urandom(args.large_mib * 1024 * 1024))
                config = f'[workspace]\ncheckout = "{mode}"\n'
                if seed:
                    copy_mode = "auto" if mode == "cow" else "copy"
                    config += f'copy = ["cache"]\ncopy_mode = "{copy_mode}"\n'
                (root / ".dwm.toml").write_text(config)
                before = free(root)
                samples = []
                paths = []
                for i in range(args.workspaces):
                    start = time.monotonic()
                    out = run(root, str(binary), "new", f"bench-{i}", "--at", revisions[i % len(revisions)])
                    elapsed = time.monotonic() - start
                    path = Path(out.stdout.strip())
                    assert path.is_dir()
                    assert not run(path, "git", "status", "--porcelain").stdout
                    paths.append(path)
                    samples.append({"seconds": elapsed, "report": out.stderr.splitlines()})
                after_create = free(root)
                for path in paths:
                    with (path / "large-0").open("r+b") as stream:
                        stream.write(os.urandom(1024 * 1024))
                after_edit = free(root)
                for path in paths:
                    run(root, "git", "worktree", "remove", "--force", str(path))
                after_delete = free(root)
                results.append({"mode": mode, "seed": seed, "source_allocated_bytes": baseline - before,
                                "created_bytes": before - after_create,
                                "edit_extra_bytes": after_create - after_edit,
                                "remaining_after_delete_bytes": before - after_delete,
                                "samples": samples})
                shutil.rmtree(root)
        print(json.dumps({"fixture": {"workspaces": args.workspaces, "small_files": args.small_files,
                         "large_mib_each": args.large_mib, "different_revisions": args.different_revisions},
                          "measurement": "volume free-space deltas; unrelated writes and delayed reclamation introduce noise",
                          "results": results}, indent=2))


if __name__ == "__main__":
    main()

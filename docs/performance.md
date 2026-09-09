# Large-repository status benchmark

## ghg, 2026-09-09

Measured on macOS arm64 with jj 0.38.0 and Rust 1.95.0. Both binaries used
`cargo build --release`. Baseline: `98501064` (the previous listing optimization).
Candidate: the change removing `latest(...)` from the jj description fallback.

The repository had 29,992 tracked files and 214,672 commits in its jj index.
It had 14 jj workspaces; dwm displayed the main checkout and three managed
workspaces. These measurements cover `dwm status`, which uses the same listing
routine as the interactive picker. They do not measure picker rendering,
preview generation, or workspace creation/deletion.

One warmup per binary, followed by seven measurements per binary, alternating
execution order. Normal jj working-copy snapshotting remained enabled. These
are warm-cache measurements on a live local checkout, not cold-cache results.

| Release binary | Median | Minimum | Maximum |
| --- | ---: | ---: | ---: |
| Baseline | 1.710 s | 1.578 s | 1.803 s |
| Optimized | 0.602 s | 0.580 s | 0.803 s |

Median speedup: **2.84×**, or **64.8% less elapsed time**. All measured status
outputs were byte-for-byte identical between and within both versions.

Raw elapsed seconds, in each binary's measurement order:

- Baseline: 1.794206500, 1.772988958, 1.578352792, 1.803359917,
  1.658274875, 1.710034291, 1.700391792.
- Optimized: 0.716408625, 0.580183084, 0.596242375, 0.803410750,
  0.697946958, 0.602216709, 0.595554250.

## What changed

When a workspace has an empty description, dwm searches its ancestors for a
description to display. The old revset wrapped that search in `latest(...)`,
which selects by committer timestamp. It must inspect the whole ancestry even
when `jj log` is limited to one result. Individual probes on ghg took roughly
one second. Removing that wrapper lets `jj log --limit 1` stop at the first
matching revision in its reverse topological history order; isolated probes
took roughly 70–150 ms and selected the same commits in all four displayed
workspaces.

This deliberately changes fallback ordering: descendants precede parents,
even if an ancestor has a newer timestamp. At merges, jj's log ordering chooses
between branches; this is not a first-parent-only search. The optimization
does not add a persistent cache or change working-copy snapshot behavior.

New integration tests cover an entirely undescribed history, multiple empty
commits, multiline descriptions, missing workspaces, and a descendant whose
timestamp predates its ancestor. All 300 tests passed. `cargo fmt --check` and
`cargo clippy -- -D warnings` passed. The additional
`cargo clippy --all-targets -- -D warnings` check found two existing test-only
lints: `let_unit_value` in `src/shell.rs` and `enum_variant_names` in
`src/workspace.rs`.

## Reproduce

Save a release build from the baseline revision before building the candidate,
then run:

```sh
python3 scripts/benchmark-status.py /path/to/repo /path/to/baseline-dwm target/release/dwm
```

The script uses only Python's standard library. It runs normal `status`
commands, including jj's usual main-working-copy snapshot, and emits JSON
with samples, medians, ranges, speedup, and output hashes. Keep repository
activity low during measurement. Relative ages and agent activity can change
the output hashes even when VCS results are unchanged. No workspace output
is included in the JSON.

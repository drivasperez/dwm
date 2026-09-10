# Copy-on-write workspaces: implementation plan

Status: implemented with experimental Git checkout sharing kept opt-in.

Implemented: revision handling, shared native clone/copy layer, ignored-file
seeding for Git and Git-backed jj, independent Git index population, integration
tests, benchmark harness, and user documentation. Local APFS results are in
[performance.md](performance.md). XFS coverage is configured in CI but has not
been run locally.

Decisions made during implementation:

- Native cloning uses a small libc wrapper; no external worktree tool is required.
- Git reads the target into a fresh index, clones candidate regular files,
  refreshes to identify mismatches, removes only those private mismatches, then
  lets `checkout-index` write missing files. The final index/status is verified.
- Post-registration failures retain a locked partial worktree for inspection.
  Automatic deletion was avoided because it could discard unexpected concurrent
  user writes. Ordinary preflight fallback occurs before registration.
- Copied files must be ignored at both source and destination revisions.
- jj has sparse-pattern controls but no established supported handoff for this
  population/index technique. Tracked checkout sharing remains Git-only; jj's
  existing workspace creation is retained. The experimental simple jj backend
  cannot use the Git-backed ignore discovery path.
- Measurements show reduced allocation but additional verification time, so the
  default remains standard checkout creation.

The original staged design follows for context.

## Goal and scope

Reduce the physical disk allocated by additional dwm workspaces while retaining normal, immediately usable Git worktrees and jj workspaces. Keep `new`, the picker, shell navigation, and workspace deletion familiar. Copy-on-write is a storage optimization; it does not defer workspace creation.

Ship in two increments: explicit copying of selected ignored files for both backends, then experimental sharing of tracked checkout files in Git. Native APFS cloning and Linux reflinks are the initial targets. Unsupported filesystems use ordinary copying or ordinary VCS checkout. No mounts, daemon, hardlinks, symlink-based sharing, or immutable baseline cache in the initial implementation.

## 1. Establish correctness and measurement baselines

- Fix `GitBackend::workspace_add` ignoring `at`. Resolve explicit revisions to full commit IDs; obtain full IDs separately from the eight-character display fields in `WorkspaceInfo`.
- Document starting-state semantics: Git forks committed state, without staged or unstaged source edits; preserve jj's existing revision semantics, including its working-copy snapshot behavior. Do not use filesystem cloning to redefine either.
- Add real Git regression coverage for default creation, `--at`, and `--from`, including a source revision different from main. Record existing jj behavior before optimizing it.
- Build an opt-in benchmark script for ordinary creation and later CoW modes. Use fixtures with many small files, a few large files, and a prepared ignored directory. Measure cold and repeated creation, physical volume allocation, and allocation after edits and deletion. Include five workspaces based on one revision and workspaces based on different revisions.
- Use a dedicated test volume where practical. Directory byte counts alone cannot establish CoW savings, and unrelated activity makes free-space deltas noisy. Report source/baseline costs and metadata overhead separately. Do not put flaky disk-allocation thresholds in unit tests.

Deliverable: an ordinary-workspace reference against which both behavior and savings can be compared.

## 2. Add a reusable native clone-or-copy layer

Add `src/fs_copy.rs` behind a small interface returning whether a file was cloned or copied, plus logical bytes processed. Evaluate a maintained Rust reflink wrapper against direct platform calls before selecting a dependency.

- APFS: native clone operation. Linux: native file reflink. Probe actual source/destination support rather than guessing from the operating system.
- Fall back only for unsupported operations or cross-filesystem copies. Permission failures, disk exhaustion, and unexpected I/O errors remain errors.
- Preserve file contents, executable permissions, and required timestamps. Copy symlinks as symlinks without traversing them; reject unsupported special files. Do not use hardlinks as a fallback.
- Create files without overwriting existing destinations; remove only partial files created by this operation. Bound recursion and exclude `.git`, `.jj`, and `.dwm`, including dwm's nested workspaces.
- Aggregate cloned/copied counts. Do not label logical cloned bytes as exact physical bytes saved.

Tests: writes in either clone remain isolated; deletion of the source leaves the clone usable; modes and symlinks survive; fallback produces equivalent files; disk and permission errors are not swallowed. Exercise native paths on APFS and a Linux reflink filesystem, and fallback on ext4. Unit tests use injected outcomes where platform support is unavailable.

## 3. First release: seed selected ignored files

Introduce a small project configuration model in `src/config.rs`, moving `.dwm.toml` parsing out of the hook-only schema while preserving current hook precedence and `conductor.json` fallback behavior.

Proposed configuration:

```toml
[workspace]
copy = ["node_modules", "target"]
copy_mode = "auto" # "auto" tries CoW, "copy" disables it

[scripts]
setup = "npm install"
```

`copy` defaults to empty. The first version accepts literal relative files/directories; pattern matching and `.worktreeinclude` interoperability are follow-ups. The copy source is the main repository by default and the explicitly named source for `--from`.

Creation order in `workspace.rs`:

1. Parse configuration, resolve the VCS revision and copy source, and validate selected paths before provisioning.
2. Create the ordinary workspace through its VCS backend.
3. Copy only eligible ignored files under the configured paths using the shared layer.
4. Run the existing setup hook, then return the workspace path on stdout.

Eligibility must use backend-appropriate ignore/tracked-file discovery, not a hand-written approximation of ignore rules. Reject absolute paths, parent traversal, VCS metadata, and source paths reached through symlinked ancestors. Never overwrite tracked destination files, even if the same path was ignored at the source revision. Missing configured paths are reported and skipped; conflicting or unsafe selections are errors.

A copy failure leaves the newly created workspace available for inspection, returns an error, and does not run setup or emit a success path. Existing setup-hook failure semantics remain unchanged. Do not automatically delete a workspace after user hooks have run.

Copied artifacts are a starting point, not proof that dependencies are current. Setup still reconciles them with the destination revision. Avoid promising portable virtual environments or universally reusable build caches. Copying from a running build is not a consistent snapshot; document that the source artifacts should be quiescent during copying.

Report a concise stderr summary, for example: `Copied 4,812 files: 4,800 cloned, 12 copied`. Preserve stdout exclusively for the shell path.

Release gate: Git and jj integration tests cover creation order, ignored-file filtering, destination conflicts, isolation, source deletion, `--from`, and configured hooks. Demonstrate reduced physical allocation for repeated copies on a supported filesystem.

## 4. Experimental Git checkout sharing

Prototype behind `workspace.checkout = "cow"`; default remains `"standard"` until compatibility is proven. This setting concerns tracked files separately from ignored-file copying. jj should reject an explicit request for this experimental mode with a clear unsupported message, rather than silently claiming it applied.

Use git-sprout as the principal implementation reference. Evaluate its population/index approach and compatibility tests; do not introduce an external executable dependency without a separate decision.

Proposed flow:

1. Resolve the requested target commit and choose a reusable source checkout: explicit `--from` first, otherwise the main checkout. Additional source selection is a later optimization.
2. Preflight whether the repository/flags fit the supported fast path. Initially fall back for filters, LFS, checkout conversions, submodules, sparse/split indexes, and other unproven cases.
3. Register a normal Git worktree without populating it.
4. CoW-clone eligible tracked files whose contents and modes match the target; let Git materialize the remaining files.
5. Establish and refresh the destination's own index. Verify behavior against ordinary Git creation before any setup hook executes.
6. Continue through the shared ignored-file copy and setup stages.

The spike must settle the exact Git plumbing for steps 4–5 before production implementation. Never copy `.git` links or administrative state from the source. Source cleanliness cannot be assumed from a short status check: clone to the private destination, verify its contents against the target for supported cases, and replace mismatches through Git. This must handle source edits racing with creation without importing them or mutating the source.

Preflight fallback should happen before registration. Failures after registration need explicit cleanup of only artifacts owned by this attempt; do not blindly rerun `worktree add` over a partial workspace. Track branch ownership and retain unexpected user data. Test name races and interruptions.

Release gate: differential tests against `git worktree add` for contents, modes, index state, status, hooks, commits, rename, and deletion. Cover dirty/racing sources, different revisions, unusual filenames, symlinks, conversions, and fallback. Record timestamp differences that could affect build tools. Enable by default only after meaningful disk savings and acceptable overhead are reproduced.

## 5. jj tracked-file sharing and rollout

The ignored-file feature already benefits jj. Treat tracked-file sharing as a separate investigation: determine how to populate files while keeping jj's working-copy bookkeeping, snapshots, and stale-workspace handling correct. Do not copy `.jj` directories or assume Git's index technique transfers to jj. If no supported approach emerges, retain ordinary `jj workspace add` and document the scope accurately.

Suggested PR sequence:

1. Git revision correctness and measurement harness.
2. Native copy layer plus opt-in ignored-file seeding, tests, and documentation.
3. Experimental Git checkout sharing and differential tests.
4. Benchmark results and a default-policy decision; jj checkout work separately.

For each shipped behavior change, update `README.md` and `site/index.html`; add measured results to `docs/performance.md`. Run formatting, `cargo t`, and `cargo clippy`, plus the relevant native-filesystem integration checks. Normal fallback must remain useful and observable.

## Prior art

- [Conductor worktrees](https://www.conductor.build/docs/concepts/git-worktrees): normal worktrees and selected ignored-file copying; public documentation does not establish checkout-level CoW.
- [Worktrunk copy-on-write](https://worktrunk.dev/step/#copy-on-write): selected ignored artifacts, native reflinks, and ordinary-copy fallback.
- [git-sprout](https://github.com/alltuner/git-sprout): sharing tracked files from an existing checkout, Git compatibility fallbacks, and differential testing.
- [simgit](https://github.com/abendrothj/simgit): cached immutable baselines and native CoW; useful comparison, but a cache introduces storage and pruning work avoided by this proposal.

Published benchmark claims from these projects are motivation, not dwm performance guarantees.

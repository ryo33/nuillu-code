# Security

Nuillu Code requires a non-bare Git repository with a checked-out branch. All coding tools share
one boundary and are confined to a per-process worktree under
`<repository-root>/.nuillu/worktrees/`; the parent is never a coding-tool root.

## Workspace boundary

- Absolute paths, `..`, `.`, and every `.git` path are rejected.
- Symbolic links are never followed by a coding tool, even when their targets are inside `cwd`.
- Hidden files are visible unless ignored.
- ripgrep's normal `.gitignore`, `.ignore`, `.rgignore`, and global ignore behavior is preserved.
- Ignored files cannot be searched, listed, read, or patched.
- The application provides no shell, arbitrary command, Git, environment, or network tool.

Startup fails unless the workspace root `.gitignore` explicitly contains `.nuillu/` (or the
equivalent root-anchored form). This prevents local model configuration, memory, code excerpts, and
logs from accidentally entering the repository. Coding tools cannot inspect `.nuillu` because it
is ignored; Nuillu's state subsystem accesses it directly.

## Child-process execution

Search and Git coordination resolve their executables once, reject executables inside the
workspace or repository, and revalidate path, size, modification time, and (on Unix) device and
inode identity before every invocation. Git runs with an empty environment, no system/global
configuration, hooks, external diff, text conversion, fsmonitor, prompting, or commit signing.
Repositories with custom clean/smudge filters are rejected; Git LFS is outside the v1 boundary.

Each ripgrep invocation receives an empty environment, `--no-config`, fixed arguments, a null
stdin, bounded output, and a ten-second timeout. Git invocations use fixed arguments and null stdin;
operations that consume a generated patch receive only that patch through a pipe.

The application itself does not constrain model-provider networking. That behavior belongs to the
user's Nuillu model set. Coding tools and the embedded visualizer transport do not provide network
access.

## Tool limits

Tool output limits are intentionally small:

- `search`: 20 matches, with at most 1 KiB retained per match
- `files`: 100 paths
- `read`: 40 lines and 32 KiB

Reading and patching also reject binary, non-UTF-8, and files larger than 16 MiB. Every bounded
result explicitly reports truncation.

## Workspace modes

Every run starts in Read-only and never persists the selected mode.

- Read-only rejects patch calls without changing either worktree.
- Review commits each internal patch as `Nuillu Code <agent@nuillu.invalid>` and returns
  immediately. The UI can inspect, apply, or discard independent commits in any order.
- Write creates the same internal commit, applies its diff to the parent working tree, then removes
  the temporary commit. It never commits on the user branch or changes the parent index.

Review-to-Write first applies the review queue as one transaction. Apply operations synchronize
the parent first. If conflict resolution changes a reviewed diff, application stops until it is
reviewed again. A failed Write transaction reverses only its own diff and discards its commit.

Parent synchronization occurs only at startup, a code-module trigger (cognition or UI control),
and immediately before a patch call. The startup snapshot establishes the observation baseline and
is not emitted as sensory input. There is no filesystem watcher or timer polling.

## Git conflict isolation

Review commits are replayed onto each parent snapshot. Git determines whether a new patch applies
independently; dependent components are recommitted together. A conflict is given to a separate
Premium-tier resolver for at most 12 tool calls. Its tools expose only Git-reported conflict paths
and exact replacements. Success is recommitted for review. Failure discards only that
self-contained commit and publishes its full diff and reason as workspace sensory input.

## Patch transactions

A patch call is structured JSON rather than a unified diff. It can contain at most 32 operations:

- `create`: a new UTF-8 text file
- `update`: exact text replacements plus a required preimage SHA-256
- `delete`: a required preimage SHA-256
- `rename`: source, destination, and a required source SHA-256

All paths in one call must be distinct. Every operation is validated before writing. Writes use
same-filesystem temporary files and no-clobber creation; a runtime failure rolls the whole call
back to its captured preimages. The UI diff is generated from the structured operations and is not
parsed back as authority.

An update renders only its changed lines plus three lines of context. Displaying both versions in
full would push the actual change past the diff display limit on a large file, leaving the user
approving a change that is not on screen.

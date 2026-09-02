# Security

Nuillu Code deliberately exposes a small set of workspace operations. All coding tools share the
same boundary and are confined to the canonical `--cwd` directory.

## Workspace boundary

- Absolute paths, `..`, `.`, and every `.git` path are rejected.
- Symbolic links are never followed by a coding tool, even when their targets are inside `cwd`.
- Hidden files are visible unless ignored.
- ripgrep's normal `.gitignore`, `.ignore`, `.rgignore`, and global ignore behavior is preserved.
- `--no-require-git` makes ignore files effective even when `cwd` is not a Git worktree.
- Ignored files cannot be searched, listed, read, or patched.
- The application provides no shell, arbitrary command, Git, environment, or network tool.

Startup fails unless the workspace root `.gitignore` explicitly contains `.nuillu/` (or the
equivalent root-anchored form). This prevents local model configuration, memory, code excerpts, and
logs from accidentally entering the repository. Coding tools cannot inspect `.nuillu` because it
is ignored; Nuillu's state subsystem accesses it directly.

## Ripgrep execution

`rg` is the only child executable. It is found through `PATH` at startup, symlinks are resolved,
the final executable must be outside `cwd`, and its file identity is checked before every run.

Each invocation receives an empty environment, `--no-config`, fixed arguments, a null stdin,
bounded output, and a ten-second timeout.

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

## Write mode

Write mode always starts off and is never persisted. Only the user can change it through the egui
toggle.

The patch module still runs while write mode is off. It validates the structured proposal, derives
a diff, pauses that module's tool call, and displays the proposal in the UI. At most one proposal
can be pending. Turning write mode on applies that exact proposal automatically; Reject discards
it. Before applying, every source is read again and its SHA-256 preimage is checked again.

While write mode is on, valid patches apply automatically without per-patch confirmation. Turning
write mode off stops subsequent writes. Stop rejects a pending patch, disables write mode, cancels
the embedded runtime, and is terminal for that app run.

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

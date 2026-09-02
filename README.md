# Nuillu Code

Nuillu Code is a deliberately small coding agent with a native `eframe` UI. The Nuillu runtime
and `nuillu-visualizer-egui` run in the same process and communicate only through in-process
channels. The application never opens a listening socket.

Every run requires a non-bare Git repository with a checked-out branch. The agent works in an
ignored, per-process Git worktree and exposes Read-only, Review, and Write modes. Review keeps
agent changes as commits until the user applies or discards them; Write applies each temporary
agent commit to the parent working tree without committing or changing its index.

The agent has exactly four workspace tools:

- `search`: ripgrep regular-expression search
- `files`: ripgrep file listing
- `read`: bounded UTF-8 file reading
- `patch`: structured create, update, delete, and rename transactions

For details about the application and its trust boundaries, see:

- [Design](docs/design.md) — architecture, module responsibilities, state, and personality
- [Security](docs/security.md) — workspace confinement, tool limits, and write-mode behavior

## Running

Requirements:

- the pinned Rust toolchain from `rust-toolchain.toml`
- `rg` available through `PATH`
- `<repository-root>/.gitignore` containing `.nuillu/`
- `<repository-root>/.nuillu/model-set.eure`
- optional memory seed Eure files under `<repository-root>/.nuillu/memory-seeds/`

Run from this repository against the current directory:

```console
cargo run -p nuillu-code -- --cwd .
```

`--cwd` may name any directory inside the repository; Nuillu Code discovers the repository root.
Git LFS and custom clean/smudge filters are rejected because startup must not execute
repository-configured processes.

To back up the existing agent database and start with a fresh one, pass Nuillu's standard flag:

```console
cargo run -p nuillu-code -- --cwd . --fresh-agent-db
```

The model set uses Nuillu's standard format and may select any provider supported by the pinned
Nuillu/Lutum versions. Provider credentials and endpoints belong only in ignored local state; do
not add them to this public repository.

## Verification

The automated suite does not call a live model:

```console
cargo test --workspace --offline
cargo check --workspace --offline
cargo fmt --check
```

Tests cover Git snapshots and index preservation, Review and Write application, dependency
grouping, conflict replay, startup-baseline and sensory-origin tracking, branch changes, executable
identity, glob semantics and worst-case matching cost, ignore behavior, path escape and symlink
rejection, output bounds, preimage conflicts, duplicate-path and ignored-destination rejection,
exact replacement counts, transaction rollback, changed-region diff rendering, duplicate tool
calls, and redaction of tool content from cognitive observations.

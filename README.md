# Nuillu Code

Nuillu Code is a deliberately small coding agent with a native `eframe` UI. The Nuillu runtime
and `nuillu-visualizer-egui` run in the same process and communicate only through in-process
channels. The application never opens a listening socket.

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
- `<cwd>/.gitignore` containing `.nuillu/`
- `<cwd>/.nuillu/model-set.eure`
- optional memory seed Eure files under `<cwd>/.nuillu/memory-seeds/`

Run from this repository against the current directory:

```console
cargo run -p nuillu-code -- --cwd .
```

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

Tests cover path escape rejection, glob semantics and worst-case matching cost, ignore behavior
outside Git worktrees, hidden files, symlink rejection, output bounds, exact replacement counts,
preimage conflicts, duplicate-path rejection, ignored-destination rejection, transaction rollback,
pending-patch approval, changed-region diff rendering, duplicate tool calls compared by value, and
the redaction of tool content from cognitive observations.

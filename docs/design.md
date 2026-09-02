# Design

Nuillu Code embeds the Nuillu server runtime and `nuillu-visualizer-egui` in a native `eframe`
application. They run in the same process and communicate only through in-process channels; the
application never opens a listening socket.

## Components

`crates/code` contains one Nuillu `code` module, all four workspace tools, and their shared safety
boundary. One persistent Premium-tier LLM session owns the module.

Every cognition activates the code module; the runtime already withholds the module's own entries.
Whether a cognition is a coding request is the model's judgement, made by calling
`leave_finding_unchanged`, rather than a pattern match over the cognition text. The code module is
the only module that selects workspace tools.

After every tool result it publishes a short cognitive observation so the wider Nuillu agent
experiences the files it listed, searches it ran, code it read, and patches it applied. These
observations are built from the typed tool result and carry bounded metadata, never project
contents. The module also publishes one final result for Nuillu's speaking path.

`crates/nuillu-code` is the application crate. It embeds the Nuillu runtime and visualizer and owns:

- the user-only write-mode toggle
- the pending-patch UI
- patch history
- the terminal Stop control

The four tools exposed by the code module are:

- `search`: ripgrep regular-expression search
- `files`: ripgrep file listing
- `read`: bounded UTF-8 file reading
- `patch`: structured create, update, delete, and rename transactions

See [Security](security.md) for the shared workspace boundary, tool limits, and patch-approval
semantics.

## State and personality

Nuillu's state directory is fixed to `<cwd>/.nuillu`:

```text
.nuillu/
├── agent.db
├── model-set.eure
├── memory-seeds/
│   └── *.eure
└── llm-logs/
```

The agent's identity and personality come from Nuillu memory seeds under
`.nuillu/memory-seeds/**/*.eure`. Nuillu persists memory, module sessions, and conversation state
in `.nuillu/agent.db`; the application imposes no additional retention or size limit.

Startup fails unless the workspace root `.gitignore` explicitly contains `.nuillu/` (or the
equivalent root-anchored form). Coding tools cannot inspect `.nuillu` because it is ignored;
Nuillu's state subsystem accesses it directly.

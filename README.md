# Rodu

Local-first, AI-first work tracking: kanban and sprints for small teams. Your data lives on your
machine, and agents are first-class users through MCP.

> Status: v0. Single-user, local SQLite. CLI, MCP over stdio and a local web board. Sync comes later.

## Install

One self-contained `rodu` binary (SQLite and the web board are inside); nothing else to install.

| | |
|---|---|
| macOS | `curl -fsSL https://raw.githubusercontent.com/TheDevper/rodu/v0.3.0/packaging/install.sh \| sh` |
| Windows (PowerShell) | `irm https://raw.githubusercontent.com/TheDevper/rodu/v0.3.0/packaging/install.ps1 \| iex` |

The script URLs name a release tag, so they run that release's reviewed script. Builds exist for
macOS (Apple silicon and Intel) and Windows x64, which also runs on Windows on ARM. Other
platforms can build from source. The binaries are not signed with a paid certificate: the scripts
install without a prompt, but a binary downloaded through a browser gets a Gatekeeper or
SmartScreen warning.

## Quick start

In any folder you want to track work in:

```sh
rodu init --name your-name --key DEMO --title "Demo project"
rodu add Fix login crash --type bug --priority urgent --assignee me
rodu ls "assignee = me() ORDER BY priority"
rodu show DEMO-1
rodu mv DEMO-1 "In Progress"
rodu web        # opens the board in your browser
```

`init` creates `.rodu/` (database and `config.json`) in the current directory. Commands use the
nearest `.rodu/` above the working directory, or `$RODU_DIR`.

## Web board

`rodu web` serves the board on `127.0.0.1` only (port 4870, or `--port`) and opens
`http://127.0.0.1:4870/#token=...`. Every API call needs that per-run token. Drag cards between
columns to change status (workflow rules apply, and refusals say how to fix them), drag within a
column to reorder, filter with JQL-lite, and click a card to edit it or comment.

## Use it from an agent (MCP)

Add Rodu to Claude Code from the directory that holds `.rodu/`:

```sh
claude mcp add rodu -- rodu mcp
```

Tools: `search`, `get_my_work`, `get_context`, `create_items`, `update_item`, `transition`,
`comment`, `link`, `list_collections`, `cycle_report`, `plan_cycle`; resource `rodu://schema`.
Every change an agent makes is recorded as "owner via agent". Workflow rules are enforced in the
domain, and refusals carry a hint the agent can act on.

Only run `rodu mcp` against a workspace you trust: it serves whatever `.rodu/` it finds.

## Query language (JQL-lite)

```
assignee = me() AND category != done AND updated > -7d ORDER BY priority
text ~ "login" AND type IN (bug, story)
cycle = currentCycle() AND assignee IS EMPTY
```

Fields: key, title, body, text, type, status, category, priority, estimate, assignee,
collection, cycle, parent, created, updated, due.

## Build from source

Requires Rust (stable) through [rustup](https://rustup.rs/).

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --locked --version <the wasm-bindgen version in Cargo.lock>
cargo xtask web                       # builds the board into crates/rodu-web/dist
cargo build --release -p rodu         # target/release/rodu, with the board embedded
```

## Layout

| Crate | Role |
|---|---|
| `crates/rodu-core` | Domain model, workflow rules, JQL-lite parser, `RoduService`, `Store` trait |
| `crates/rodu-store` | SQLite store with FTS5, JQL-lite to SQL compiler |
| `crates/rodu-sync` | Replicated document for team sync, on Loro ([ADR 0001](docs/adr/0001-sync-crdt.md)) |
| `crates/rodu-api` | JSON contract shared by the server and the board |
| `crates/rodu-http` | Local API and static files for the web board |
| `crates/rodu-mcp` | MCP server |
| `crates/rodu-web` | Kanban board (Leptos, compiled to WebAssembly) |
| `crates/rodu` | `rodu` command |
| `xtask` | Build, packaging and smoke-test tasks |

## Releasing

```sh
cargo xtask dist             # dist/release: the archive for this machine (or --target NAME)
cargo xtask smoke            # drives the binary built for this machine
cargo xtask release-files    # SHA256SUMS, rodu.rb, rodu.json and the install scripts
```

CI (`.github/workflows/ci.yml`) tests on macOS and Windows, builds the binaries, installs each one
with the user-facing script on its own OS and smoke-tests it. Pushing a `vX.Y.Z` tag that matches
the version in `Cargo.toml` and the install URLs above publishes a GitHub release.

## Contributing

Every commit needs a `Signed-off-by` line (`git commit -s`): see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache License 2.0](LICENSE). Copyright 2026 TheDevper. The license does not grant use of the
Rodu name. The release archives also carry `THIRD-PARTY-NOTICES.txt` for the Rust crates and
SQLite built into the binary.

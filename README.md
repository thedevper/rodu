# Rodu

Local-first, AI-first work tracking: kanban and sprints for small teams. Your data lives on your
machine, and agents are first-class users through MCP.

> Status: v0. Local SQLite, CLI, MCP over stdio and a local web board. Teams sync through a shared
> folder, optionally encrypted and with each machine admitted by the team's owner.

## Install

One self-contained `rodu` binary (SQLite and the web board are inside); nothing else to install.

| | |
|---|---|
| macOS | `curl -fsSL https://raw.githubusercontent.com/TheDevper/rodu/v0.4.0/packaging/install.sh \| sh` |
| Windows (PowerShell) | `irm https://raw.githubusercontent.com/TheDevper/rodu/v0.4.0/packaging/install.ps1 \| iex` |

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

## Work as a team

Share a workspace through a folder your team already syncs (Google Drive, Dropbox, OneDrive,
iCloud Drive or Syncthing). Rodu only reads and writes files there; the folder's own app moves
them between machines.

```sh
rodu team create --folder ~/Drive/our-board --encrypt   # prints an invite code
# on a teammate's machine, with the same folder synced; paste the code when asked:
rodu team join - --folder ~/Drive/our-board --name bob
```

After that every command syncs by itself, and `rodu sync` does it by hand. New cards made on a
teammate's machine get a key such as `DEMO-KQMRTZ` until the machine that created the team
gives them their number; the old key keeps working.

With `--encrypt`, every file in the folder is sealed with a team key, so the folder's provider
cannot read the board. The invite code holds that key: share it like a password, and see it
again with `rodu team --show-invite`. Anyone with the code and the folder can read and change
the board. With `--no-encrypt`, anyone with access to the folder can read it.

With `--signed`, every machine signs what it writes, and the team takes in changes only from
machines its owner or an admin admitted. A machine that joins prints its code; check it with the
person, then admit it:

```sh
rodu team create --folder ~/Drive/our-board --encrypt --signed
rodu team admit                 # machines asking to join
rodu team admit bob <code>      # the code bob's machine printed
rodu team members               # who is on the team
rodu team remove bob --yes      # refuse what bob's machines write from now on
```

On an encrypted signed team, `team remove` also changes the team key, so the removed machine
cannot read what is written after it. A team made without `--signed` cannot be switched to it
later. `rodu team admin` lets another person admit machines, and `rodu team transfer-owner`
hands the team over.

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

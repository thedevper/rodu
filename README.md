# Rodu

Local-first, AI-first work tracking: kanban and sprints for small teams. Your data lives on your
machine, and agents are first-class users through MCP.

> Status: v0. Local SQLite, CLI, MCP over stdio and a local web board. Teams sync through a shared
> folder, optionally encrypted and with each machine admitted by the team's owner.

## Install

One self-contained `rodu` binary (SQLite and the web board are inside); nothing else to install.
Builds exist for macOS (Apple silicon and Intel) and Windows x64, which also runs on Windows on
ARM. Other platforms can build from source.

### macOS

```sh
brew install thedevper/tap/rodu
```

Or without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/TheDevper/rodu/v0.9.0/packaging/install.sh | sh
```

Update with `brew upgrade rodu`, or by running the script again.

### Windows

In PowerShell (5.1 or 7; no administrator needed):

```powershell
irm https://raw.githubusercontent.com/TheDevper/rodu/v0.9.0/packaging/install.ps1 | iex
```

It puts `rodu.exe` in `%LOCALAPPDATA%\Programs\rodu` and adds that folder to your PATH. Open a new
terminal afterwards, then check with `rodu --version`.

To update, run the same command again: it installs the latest release. It works while `rodu web`
or `rodu mcp` is running; restart them to use the new version. `$env:RODU_VERSION = '0.9.0'`
before the command installs a given release instead.

Or with [Scoop](https://scoop.sh):

```powershell
scoop bucket add thedevper https://github.com/TheDevper/scoop-bucket
scoop install rodu
scoop update rodu    # later, to update
```

Use one of the two: both on the PATH means two copies to keep up to date.

### About the scripts

The script URLs name a release tag, so they run that release's reviewed script; each checks the
download against the release's `SHA256SUMS`. The binaries are not signed with a paid certificate:
the scripts, Homebrew and Scoop install without a prompt, but a binary downloaded through a browser
gets a Gatekeeper or SmartScreen warning.

## Quick start

In any folder you want to track work in:

```sh
rodu init --name your-name --key DEMO --title "Demo project"
rodu add Fix login crash --type bug --priority urgent --assignee me
rodu ls "assignee = me() ORDER BY priority"
rodu show DEMO-1
rodu show DEMO-1 --activity   # also who changed what and when
rodu mv DEMO-1 "In Progress"
rodu web        # opens the board in your browser
```

`init` creates `.rodu/` (database and `config.json`) in the current directory. Commands use the
nearest `.rodu/` above the working directory, or `$RODU_DIR`.

A workspace can hold several collections, each with its own card numbers (DEMO-1, OPS-1):
`rodu collection add OPS --title "Operations"`, the **+** beside the collection list on the board,
or the `create_collection` MCP tool. `rodu collections` lists them, and `rodu add ... --collection
OPS` puts a card there. Everyone on a workspace's team sees all of its collections; for work that
needs different people, make a separate workspace in its own folder.

## Web board

`rodu web` serves the board on `127.0.0.1` only (port 4870, or `--port`) and opens
`http://127.0.0.1:4870/#token=...`. Every API call needs that per-run token. Drag cards between
columns to change status (workflow rules apply, and refusals say how to fix them), drag within a
column to reorder, filter with JQL-lite, and click a card to edit it or comment. In a team
workspace, **Team** shows who is on the team and their machines; on the owner's or an admin's
machine it also invites someone (copy the invite, shown once), admits a machine by the code its
person reads out, cancels an invite, and removes someone after asking. On a board that is yours
alone, **Team** shares it with a team instead: pick one of your Google Drive folders and the board
reopens, encrypted and signed, on the same link. Started in a folder with no workspace, `rodu web`
offers to join a team instead: paste the invite a teammate sent you, and the board opens on the
same link once joined.

## Work as a team

Share a board through Google Drive, with a personal account or your organisation's. No server and
nothing to pay for: Google Drive for desktop moves the files between machines, and Rodu only reads
and writes them.

```sh
# in the folder you track work in; makes a workspace there if there is none yet
rodu team create         # finds your Google Drive
rodu team invite bob     # an invite for bob; send it to him yourself
# on bob's machine, once the team's folder is in his Google Drive:
rodu team join           # paste the invite when asked; it finds the folder
```

Your machine admits bob's the next time it syncs, which any command, `rodu web` and `rodu mcp` do.
An invite admits one machine, once, within 7 days (`--days` from 1 to 90). `rodu team invites`
lists the ones not used yet, and `rodu team invite --revoke bob` cancels one. A teammate can also
join with the team's invite code from `rodu team --show-invite`; their machine then prints a code
to check with them before `rodu team admit bob <code>`.

`team create` lists the Google Drives it finds (each account's My Drive, and its shared drives in an
organisation) and puts the team's files in `Rodu/<KEY>` inside the one you pick with `--drive
<number>`. A shared drive suits an organisation: every member sees it at the same place, and members
should have the Content manager role (the default for new members), because a Contributor cannot
remove files and Rodu removes its own old ones. In a personal My Drive, share the `Rodu/<KEY>`
folder with your team in Google Drive. Each teammate then adds a shortcut to it in their My Drive,
so it syncs to their machine. Another folder app (Dropbox, OneDrive, iCloud Drive, Syncthing) works
too, given its folder: `--folder <path>`.

After that every command syncs by itself, and `rodu sync` does it by hand. New cards made on a
teammate's machine get a key such as `DEMO-KQMRTZ` until the machine that created the team
gives them their number; the old key keeps working.

A team is encrypted: every file in the folder is sealed with a team key, so Google cannot read the
board. Invites and the invite code hold that key: share them like a password. Every machine also signs what it writes, and the team takes in changes only from
machines its owner or an admin admitted:

```sh
rodu team admit                 # machines asking to join
rodu team members               # who is on the team
rodu team remove bob --yes      # refuse what bob's machines write from now on
```

`rodu team members` and the board's **Team** panel also say when each teammate's machine last
synced, whether it then had everything your machine has, and which Rodu version it runs, so a
teammate who reads the board without changing anything still shows up. Each machine keeps that in
a small file in its own folder, sealed and signed like the rest; machines on Rodu 0.7.0 or older
write none.

`team remove` also changes the team key. What each machine writes once it has the new key is
closed to the removed machine. What the folder held before stays readable to it, and so does what
a machine writes before it next syncs. `rodu team admin` lets another person admit machines, and
`rodu team transfer-owner` hands the team over. `--no-encrypt` leaves the files readable to anyone
with access to the folder, and `--unsigned` lets anyone with the invite code in without being
admitted.

## Use it from an agent (MCP)

Add Rodu to Claude Code from the directory that holds `.rodu/`:

```sh
claude mcp add rodu -- rodu mcp
```

Tools: `search`, `get_my_work`, `get_context`, `get_activity`, `get_audit`, `create_items`, `update_item`, `transition`,
`comment`, `link`, `list_collections`, `create_collection`, `cycle_report`, `plan_cycle`; resource `rodu://schema`.
Every change an agent makes is recorded as "owner via agent", and `get_activity` (or
`rodu show --activity`, or Activity on a card in `rodu web`) lists who changed what on a card and
when. In a team it lists every machine's changes, read from the team's shared history: times are
each machine's own clock, so one that runs ahead shows as "before" the moment yours received it.
`rodu show --activity --local` lists what this machine recorded itself. To check the record,
`rodu audit DEMO-1` (or `get_audit`) lists each change Rodu reads as touching a card, with the
machine that wrote it, its raw times and commit message. It reads the team document's history
each time, not the activity index, and nothing in Rodu writes to it; when this machine received
each change, and how the team stands with each machine, are this machine's own notes. Workflow rules are enforced
in the domain, and refusals carry a hint the agent can act on.

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
cargo xtask browser-smoke    # drives its board in headless Chrome (Node 22+, Chrome)
cargo xtask release-files    # SHA256SUMS, rodu.rb, rodu.json and the install scripts
```

CI (`.github/workflows/ci.yml`) tests on macOS and Windows, builds the binaries, installs each one
with the user-facing script on its own OS and smoke-tests it, and drives the board in headless Chrome on macOS. Pushing a `vX.Y.Z` tag that matches
the version in `Cargo.toml` and the install URLs above publishes a GitHub release, then opens a
pull request on [homebrew-tap](https://github.com/TheDevper/homebrew-tap) and
[scoop-bucket](https://github.com/TheDevper/scoop-bucket) with its `rodu.rb` and `rodu.json`.
`brew upgrade` and `scoop update` see the release once those are merged. That step needs a
`PACKAGES_TOKEN` secret: a fine-grained token with contents and pull requests write on both
repositories. Without it CI warns, and the two files are copied over by hand.

## Contributing

Every commit needs a `Signed-off-by` line (`git commit -s`): see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache License 2.0](LICENSE). Copyright 2026 TheDevper. The license does not grant use of the
Rodu name. The release archives also carry `THIRD-PARTY-NOTICES.txt` for the Rust crates and
SQLite built into the binary.

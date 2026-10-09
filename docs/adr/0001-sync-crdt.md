# ADR 0001: CRDT library for team sync

- Status: accepted 2026-10-09 (Loro; not implemented yet)

## Context

Rodu is single-user today: each person's `.rodu/rodu.db` is their own board. To replace a hosted
tracker for a team, replicas must merge without a central server Rodu runs. The constraints that
decide the library:

1. **Transports the team owns.** A self-hosted relay, a Git remote, S3, a synced folder or the
   LAN. The library must export and import plain bytes, with no protocol of its own.
2. **A synchronous `Store`.** `rodu_core::Store` is synchronous, and every implementation keeps an
   in-process replica.
3. **One binary.** Rodu ships one self-contained executable for macOS and Windows. The library
   must compile into it natively, with nothing read from beside the binary at runtime.
4. **A CLI that starts per command.** SQLite stays the index that reads go to, but every write
   loads the workspace's document, changes it and exports an update. That path runs on every
   `rodu add`, `rodu mv` and agent write.
5. **What ships is ours to answer for.** Every crate in the binary must pass `about.toml`, and
   crates with open security advisories need a reason to be there.

## Options measured

Same 7 merge scenarios and the same card data for each library: `loro` 1.16.2, `yrs` 0.28.0 (the
Rust port of Yjs) and `automerge` 0.12.0. Release build with Rodu's profile (LTO, one codegen
unit, `panic = "abort"`), Rust 1.99.0 on an Apple M4. Timings are for 20,000 cards.

| | yrs 0.28.0 | Loro 1.16.2 | Automerge 0.12.0 |
|---|---|---|---|
| All 7 merge scenarios converge | yes | yes | yes |
| Release binary size added (upper bound) | +0.44 MB | +2.0 MB | +1.4 MB |
| CLI write: load, 1 edit, export | 27 ms | 13 ms | 300 ms |
| Load and import a teammate's update | 27 ms | 3 ms | 300 ms |
| One field edit on the wire | 321 B | 98 B | 113 B |
| Create 20k cards | 51 ms | 133 ms | 3.2 s |
| Read every card after load | 78 ms | 126 ms | 743 ms |
| Snapshot, 20k cards | 3.9 MB | 3.1 MB | 0.5 MB |
| Peak RSS of the whole run | 106 MB | 154 MB | 321 MB |
| Crates in the shipped tree | 39 | 139 | 48 |
| Licences beyond the current `about.toml` | none | MPL-2.0 (3 crates) | none |
| RustSec advisories in the shipped tree | `smallstr` unmaintained | `im`, `sized-chunks`, `bitmaps` unmaintained; soundness advisories on `im` and `sized-chunks` | none |
| Native moves and trees | no | `MovableList`, `Tree` | no |

The binary size is a minimal program using the library minus an empty program. Crates Rodu
already ships, such as `serde`, are counted in it, so the real increase is smaller. Rodu 0.3.0 is
5.4 MB.

Automerge converges correctly but is 10 to 20 times slower than the others on every path a CLI
command takes. It is not considered further.

## What no library solves for us

These showed up identically in all three libraries, so they are Rodu design work:

- **Card numbers collide.** Two people offline both create the next card, and both get `DEMO-13`.
  The library merges both cards correctly but cannot know that numbers are meant to be unique.
- **Delete beats a concurrent edit.** Removing a map entry discards an edit made at the same time.
  Rodu should never hard-delete a card; archive it with a field instead.
- **Workflow rules can break after a merge.** A moves a card to In Progress, which requires an
  assignee, while B unassigns it. Each write was valid locally, but the merge is not. Merged
  writes cannot be rejected, only reported.
- **The same field written twice keeps one value.** Each library picks a winner by peer order,
  not by time. That is fine for status and assignee if the event log records the other write.
- **Ranks can tie.** Two cards can end up with the same rank string. Ordering by `(rank, id)`
  already makes the order deterministic.
- **No access control.** Anyone who can write to the transport can write anything. That is
  acceptable for a small trusted team. Signing updates per principal is later hardening.

One more is solved by Loro but would not be by yrs or Automerge:

- **Parents can form a cycle.** A makes X a subtask of Y while B makes Y a subtask of X. Both
  writes are valid locally. Loro's `Tree` drops the move that would close a cycle on merge, the
  same way on every replica (checked: X ends under Y, Y stays at the root, both replicas equal),
  so card parents live in a `Tree`, not in a plain field.

## Decision

**Use Loro**, behind the existing `Store` trait, with SQLite kept as a derived index for queries.
yrs is the fallback if Loro's dependencies stay a problem (see below).

Why Loro:

- **Fastest on the paths a command takes.** A write (load, one edit, export) takes 13 ms at
  20,000 cards against 27 ms for yrs, and importing a teammate's update 3 ms against 27 ms.
- **Smallest updates.** One edit on the wire is 98 B, a third of yrs's, which matters for Git and
  S3 transports that keep every file.
- **`Tree` and `MovableList` fit the data.** `Tree` keeps subtasks free of cycles after a merge
  without code of ours, and shallow snapshots keep a long-lived board from growing forever.

What we take on, and how we contain it:

- **Three unmaintained crates with advisories.** `im` (RUSTSEC-2026-0248, and RUSTSEC-2023-0126
  on `OrdSet`), `sized-chunks` (RUSTSEC-2026-0251, and RUSTSEC-2026-0255 on panic safety) and
  `bitmaps` (RUSTSEC-2026-0247). Loro uses `im::HashMap` for version vectors, not `OrdSet`, and
  Rodu's release profile has `panic = "abort"`, under which the panic-safety bug cannot happen.
  We ship on that reasoning, record it in `.cargo/audit.toml` per advisory, and follow upstream pull
  request loro-dev/loro#1122, which replaces `im` with its maintained fork `imbl`. We take the
  Loro release that contains it and remove the exceptions.
- **A crafted file can crash Loro.** A fuzz test in `rodu-sync` found sync files that pass
  Loro's checksum and make it panic on import or on the first read afterwards, in release builds
  too, which aborts Rodu (reported privately to Loro on 2026-10-09). Anyone who can write to the sync folder could otherwise stop every
  teammate's Rodu. So every import from the folder is first replayed in a child process
  (`Replica::import_untrusted`) and refused if the child crashes, exits with an error, runs past a
  time limit, or the file is over 64 MB; only then does the real replica import it. The cost is a
  process start (about 7 ms) plus handing the child a snapshot of the board (export takes about
  115 ms at 20,000 cards), so the transport checks new files in batches.
- **MPL-2.0 obligations** for those crates, described under Licensing.
- **About 2 MB more binary** (about 0.4 MB for yrs). Rodu 0.3.0 is 5.4 MB.
- **100 more crates in what we ship** (139 against 39 for yrs), all covered by `about.toml` and
  `cargo audit` from now on.

## Licensing

Loro is MIT, and nearly all of its tree is MIT or Apache-2.0, with `xxhash-rust` under BSL-1.0
and `unicode-ident` under Unicode-3.0, both already accepted. The exceptions are `im`,
`sized-chunks` and `bitmaps`, under MPL-2.0 (or later). Before the first release that contains
Loro:

1. Accept MPL-2.0 for exactly `im`, `sized-chunks` and `bitmaps` (or their `imbl` successors)
   with per-crate entries in `about.toml`, not globally:

   ```toml
   [im]
   accepted = ["MPL-2.0"]
   ```

   This was checked with cargo-about 0.9.2: the build then passes, and any other MPL-2.0 crate
   still fails it.
2. Add to `THIRD-PARTY-NOTICES.txt`, for those crates, the MPL-2.0 text and where to get their
   exact source (their crates.io source archives). MPL-2.0 is file-level copyleft: it does not
   change Rodu's licence, and changes to those files would have to stay MPL-2.0. We make none.
3. Run `cargo audit` in CI, with each accepted advisory listed in `.cargo/audit.toml` with its reason,
   so any new advisory on a shipped crate fails the next build.

## The Loro store (built 2026-10-09)

`rodu_sync::LoroStore` implements `Store` over a workspace directory. The same service test suite
runs against it and against `SqliteStore`.

- **Layout.** Cards are nodes of a Loro `Tree` (`items`), their fields in the node's map and their
  parent the tree parent. Principals, collections, cycles and comments are maps from id to a
  mergeable map, so two machines writing one entity merge field by field. Links are keyed by a
  hash of what they connect, so the same link made twice is one; a key ends up in a Loro container
  name, which must not contain `/`, and only ids and these hashes are ever used as keys.
- **Local only.** Events (the audit trail), idempotency records and the number counter stay in each
  replica's `rodu.db`. A card's `version` is local too (decision 4 above). Syncing every event
  would make the document grow with every edit forever.
- **Files.** `rodu.loro` holds the document; `rodu.db` is the index plus the local data, and
  records the SHA-256 of the document file it matches. A write saves the new document as
  `rodu.loro.<sha256>.next` and puts its hash in the index in the same SQLite transaction, then
  renames it into place after the commit. Whoever next takes the write lock finishes the one the
  index names and deletes any other, so a transaction that failed never changes the document and
  one that committed is never lost. The hash is in the name because the rename after a commit
  runs outside the lock: a late rename can only move its own file, never another process's. The
  snapshot is first written whole as a `.partial` file and renamed to its `.next` name, so a file
  under that name is never torn even when the same snapshot is staged again; the directory is flushed after the file is created and after the
  rename (on Unix). If the index does not match the document on open, or `rodu.db` is gone, the
  index is rebuilt from the document.
- **One machine, several processes.** SQLite's write lock is the lock on the document: each write
  transaction first reloads the document if another process changed it, so no process writes
  operations from a stale copy under the replica's peer id.
- **Import.** `import_untrusted` replays the import in a child process first (as `Replica` does),
  then indexes the entities in the document's diff. Every entity read from the document is
  validated as strictly as a local write (one-line titles and names without control or invisible
  format characters, real dates and timestamps, known values, link targets that are a card id or,
  for a pull request, an http(s) URL); a bad one is left out and reported, and a reference the index cannot hold (an unknown
  assignee, cycle or parent) is cleared and reported. Indexing never fails on what a teammate's
  machine wrote. Only clean changes are indexed one by one: cards, comments and links that decode
  and resolve without a clash. Anything else (a principal, collection or cycle changed, a malformed
  or unresolved entity, or an index still holding something cleared or left out) rebuilds the whole
  index, so a reference cleared today returns when what it points to arrives, and an entity an
  update made malformed leaves the index. A card's local version goes up only when its row
  changes, so a rebuild (even one on every import) never makes a client's version stale.
- **Writes keep the document's values.** The index can show what the document does not hold: a
  suffixed name, a fallback key, a cleared reference. An update writes to the document only the
  fields it changed against the index row it replaced, and moves a card only when its parent
  changed, so editing a card's title never erases an assignee the index could not resolve yet.
  The cost: clearing a field the index already shows as cleared writes nothing, so the old
  reference returns once what it points to arrives. Clear it again then.
- **Clashes after a merge.** When two collections share a key, two principals a name, two cycles a
  name in one collection, two cards a number in one collection, or two cards a key, the one with
  the lowest id (the first made) keeps it. The others are shown as `OPS2`, `ann2`, `Sprint 1 (2)`,
  or for a card its provisional key or id, each taking the first candidate nothing holds yet. A
  card that lost its number counts as unnumbered, so the numbering peer gives it the next number.
  Ids made in one millisecond in one process keep their creation order (a UUIDv7 counter), so the
  rule holds within a batch. The result depends only on what the document holds, so every replica shows the same, and a
  rebuilt index equals the one kept up to date import by import (tested with three replicas doing
  random work). Each clash is reported, for the "needs attention" view.
- **Measured** at 20,000 cards, release build on an M-series Mac: the document file is 5.7 MB, one
  card edit through the service (index row and snapshot saved) takes 25 ms, opening the store
  86 ms, and a full index rebuild 0.7 s. Saving a whole snapshot per write is the simple choice for now; step
  4's update files will let a write append instead.

## The team folder (built 2026-10-09)

Step 4a: sync through a folder the team already shares, and the commands to set it up.

- **Layout.** `<folder>/rodu-team.json` names the team (`{"format": 1, "workspaceId": ...}`).
  Each replica writes only `sync/<its peer id, 16 hex>/<sequence, 10 digits>.update`, and each
  file holds only that replica's own operations since its last file. A file is framed as
  `RODU-UPDATE1`, the payload's length and its SHA-256, so a reader tells a file the folder app
  is still bringing in (shorter than its length: skipped until complete) from a damaged one
  (reported). Files are written under a hidden temporary name and renamed into place.
- **Reading the folder.** Everything in it is untrusted. Directory and file names are parsed,
  never joined into paths; links, dotfiles and files over the import cap are skipped; conflict
  copies (`0000000003 (1).update`) are read like any other file. A replica folder named for peer
  0 is ignored: Loro never gives that id. A file is the unit of import, whole or not at all. New
  files are replayed in a child process (`rodu __check-import`) in groups under the import cap;
  the child also refuses a file holding operations of any peer but the one its folder names. A
  group that passes is imported; when one is refused, each of its files is checked alone, against
  the document with the files accepted so far, so one bad file never holds the rest back. Files
  are remembered by folder, name and SHA-256: a file rewritten under a seen name is checked and
  imported again, and operations Loro already holds are ignored by their ids. A file is
  remembered as done only once every operation in it is in the log: Loro keeps operations whose
  predecessors have not arrived pending in memory, and a saved document leaves them out, so such
  a file is read again each time until they land. Each check replays the files still pending
  first, so the child applies their operations exactly when this process does. A file found invalid or damaged is remembered
  and reported once; one the child could not finish (a crash or timeout) is tried again next
  time. File names are escaped before they are shown.
- **Writing.** Under the workspace write lock, a replica exports its own operations from the
  counter it last exported to and writes them as its next file. A folder without
  `rodu-team.json` (a cloud drive not mounted) is never written to.
- **Commands.** `rodu team create --folder <path> --no-encrypt` turns the workspace in place
  into a team workspace: the document is built from every row of its index, so events,
  idempotency records and versions stay. It prints an invite code, `rodu1-<workspace id>`, which
  holds no secret yet. The folder is checked before the workspace changes: one that already names
  a team is refused, unless its only replica folder is this workspace's own, left by a create
  that stopped half way (the creator's replica folder is made before the team file for this).
  `rodu team join <code> --folder <path> --name <you>` makes a workspace from
  the folder; a join that fails removes what it made. `rodu team` shows the folder, code and role;
  `rodu sync` syncs by hand. `--encrypt` is refused until step 4b, and giving neither flag is an
  error, so there is no silent default.
- **Automatic sync.** In a team workspace `add`, `ls`, `show` and `mv` read the folder first and
  write to it after; the numbering peer (the machine that created the team) numbers new cards in
  between. `web` and `mcp` sync when they start and when they stop. A folder that cannot be read
  is a warning, and the command, `rodu sync` included, works on what the machine has.
- **Left for later steps.** 4b: encryption. 4c: the readable copy, `web` and `mcp` watching the
  folder while they run, compaction (today every command reads every file in the folder), and the
  command that hands numbering to another machine. A file waiting on operations that never arrive
  (its predecessor refused, or never written) is read and checked again on every command, with no
  limit yet; `rodu sync` lists such files. A join that fails after its principal reached
  the folder leaves that principal behind; joining again under the same name then shows `bob2`.

## Proposed design for the open problems

1. **Card numbers (decided 2026-10-09: provisional keys).** A new card gets a provisional key
   built from the random bits of its id, six letters such as `DEMO-KQMRTZ` (no digits, so it never
   looks like a real number; no I, L, O or U; growing to 8 or 10 letters if the key is taken), and
   keeps it until it is numbered. Letters only, because `~` and the other symbols are JQL-lite
   operators. Exactly one peer per
   workspace, the *numbering peer*, gives out real numbers: when it imports a provisional card, it
   assigns the next number and writes it to the card. Two peers numbering at once would collide
   again without a server, so no other peer ever numbers. The numbering peer is the one that ran
   `init`. A command can hand the role to another peer if that machine is gone for good. A real
   number never changes once given, and the provisional key keeps resolving to the card as an
   alias, so links written before the sync still work. Rejected: renumbering the later card after
   a merge, because then a key someone has already shared can point to another card.
2. **Deletes** become an `archived_at` field. Nothing is removed from the document.
3. **Rules** are checked locally at write time, as today. After every import, re-check the
   touched cards, and show any that now break a rule in a "needs attention" view. Never undo
   merged writes silently. Parent cycles are broken as described above.
4. **`expected_version`** stays a local optimistic check on this replica. It no longer means a
   global version.
5. **Transport (decided 2026-10-09: a shared folder first).** The team's sync folder is one it
   already shares: Google Drive, Dropbox, OneDrive, iCloud Drive or Syncthing. Each peer appends
   update files under `sync/<peer-id>/<sequence>.update`, and a periodic compaction writes a
   snapshot. Rodu never talks to those services; their own apps move the files. A self-hosted
   `rodu relay` comes later, for teams that want changes to arrive in under a second, carrying the
   same files. Git or S3 can carry them too.
6. **Sync is automatic (decided 2026-10-09).** Nobody runs a sync command in normal use:
   - every CLI command imports what is new in the folder before it runs and writes its own update
     file right after;
   - `rodu web` and `rodu mcp` watch the folder while they run, so a teammate's change appears on
     the board without a refresh;
   - offline work stays local and goes out the next time a command or the board runs.

   `rodu sync` remains for running a sync by hand and for diagnosing one.
7. **Joining a team.** `rodu team create --folder <path>` turns a workspace into a team workspace
   and prints an invite code holding the workspace id and, when encrypted, the team key.
   `rodu team join <code> --folder <path>` sets up a teammate's replica from the folder. The invite
   code is a secret when it holds a key, and the CLI says so.
8. **Encryption is the team's choice (decided 2026-10-09).** `team create` takes `--encrypt` or
   `--no-encrypt` and asks when neither is given, so there is no silent default.
   - Encrypted: each update file and snapshot is sealed with the team key (an authenticated
     cipher, so a changed file is refused, not merged), and the folder's provider cannot read the
     board.
   - Not encrypted: the files are plain Loro updates. They are still binary, not something a
     person reads in Drive.
   - `--readable-copy`, on either setting, also keeps a plain Markdown copy of the board (one file
     per card) in the folder, for reading it from Drive or a phone. That copy is never encrypted,
     and `team create` warns that anyone with access to the folder can read it.
   - Turning encryption on later re-keys from that point. Earlier plain files may survive in the
     provider's file history, and the CLI says so.
9. **Identity.** Each person's replica has its own peer id, and agents write through their
   owner's replica, as `via_agent_id` records today.

## Next steps

1. Do the Licensing steps above, together with adding the `loro` dependency, so the build never
   ships Loro without its notices.
2. Design provisional keys and the numbering peer in the core model. This includes the alias
   lookup and the hand-over command. It also covers a single-user workspace, where the numbering
   peer is the only peer and every card is numbered immediately, as today. Done except the
   hand-over command and the setting that turns numbering off, which need `rodu team join`
   (step 4): `Item.number` is optional, `RoduService::with_numbering` and `assign_numbers` exist,
   and SQLite schema 2 migrates schema 1 workspaces in place.
3. Build a Loro-backed store implementing `Store`, keeping the SQLite index in sync from the
   document's change events, and run the existing service tests against it. Done: `LoroStore`
   in `rodu-sync`, described under "The Loro store" above. The CLI still opens plain workspaces
   with `SqliteStore`; step 4 opens team workspaces with `LoroStore`.
4. Shared-folder transport with automatic sync, `rodu team create` and `rodu team join`, and
   the encryption and readable-copy options. Test it with two and three workspaces on one folder
   in CI on macOS and Windows, including a folder app's conflict copies (`file (1).update`) and
   half-written files. Split in three: 4a (transport, team commands, automatic sync) is done,
   described under "The team folder" above; 4b adds encryption; 4c the readable copy, live
   watching in `web` and `mcp`, compaction and the numbering hand-over.
5. Later: `rodu relay` for live sync.
6. Move to the Loro release that replaces `im` with `imbl` once loro-dev/loro#1122 lands, and
   drop the advisory exceptions.

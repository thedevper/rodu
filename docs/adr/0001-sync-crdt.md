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
  copies (`0000000003 (1).update`) are read like any other file. A replica folder named for peer 0
  is ignored: Loro never gives that id. A file is the unit of import, whole or not at all. New
  files are replayed in a child process (`rodu __check-import`) in groups under the import cap;
  the child also refuses a file holding operations of any peer but the one its folder names. A
  group that passes is imported; when one is refused, each of its files is checked alone, against
  the document with the files accepted so far, so one bad file never holds the rest back. Files
  are remembered by folder, name and SHA-256: a file rewritten under a seen name is checked and
  imported again, and operations Loro already holds are ignored by their ids. A file is remembered
  as done only once every operation in it is in the log: Loro keeps operations whose predecessors
  have not arrived pending in memory, and a saved document leaves them out, so such a file is read
  again each time until they land. Each check replays the files still pending first, so the child
  applies their operations exactly when this process does. When such a check fails, the new file
  is checked without them: if it passes, the held file that breaks once it is released is refused
  (at once when the child finds it invalid; when it crashes or hangs the child, only the second
  time in a row, in a later batch, since that may be the machine's doing; a check that releases it
  and passes clears the count), and the rest of the batch waits for the next one, which starts
  from the saved document. After any import that leaves operations pending, the next one starts
  from the saved document too, so pending operations never reach a later import unchecked. A file
  found invalid or damaged is remembered and reported once; a file the child could not finish on
  its own (a crash or timeout) is tried again next time. File names are escaped before they are
  shown.
- **Writing.** Under the workspace write lock, a replica exports its own operations from the
  counter it last exported to and writes them as its next file. A folder without
  `rodu-team.json` (a cloud drive not mounted) is never written to.
- **Not reading a file twice (built 2026-10-09, step 4c).** Once a file's content has been dealt
  with (landed, refused or reported damaged), the file is also remembered by its name, size,
  modification time and (on Unix) inode, and a later pull skips it after a `stat`, without opening
  it. A command so costs one `stat` per file instead of reading every byte in the folder. A file
  rewritten in place or replaced gets a new time or inode and is read again; only a rewrite that
  keeps the size and sets the time back, or falls within the file system's time resolution, is
  missed, and a file a reader already dealt with holds nothing it lacks. A file still waiting for other operations is not
  remembered this way, so it is read on every pull until it lands.
- **Compaction (built 2026-10-09, step 4c).** Once a replica has written 32 files since it last
  compacted, its push writes one more numbered file holding every operation it has published
  (its own counters from 0 up to the last one exported), sealed like any other for an encrypted
  team, and then removes its own lower-numbered files. It runs under the workspace write lock,
  writes before it removes, and removes only regular files named like its own numbered files: never
  conflict copies, links, dotfiles or another replica's files. Files stay one replica's own
  operations, so the import rule that a file holds only its folder's peer is kept; a whole-document
  snapshot would break it. If the merged payload would be over the 64 MiB import cap, the replica
  does not compact and tries again after as many files more. Anything that goes wrong after the
  push wrote its own file, such as an old file that cannot be removed, is a warning, not a failed
  push; the next compaction removes the file.
- **Readers during a compaction.** Readers never need to know which files a compacted one
  replaces: what links them is the operations' ids, not the file names. The compacted file covers
  every counter of its replica from 0 up to the last export, so it holds every operation of every
  file it replaces. A folder app may deliver the new file and the removals in either order:
  - new file first: the old files' operations are already in the reader's log, so Loro ignores
    them by id and the file lands at once;
  - removals first: a later file from that replica whose operations build on removed ones has
    operations pending. It is the waiting case above: the file is not marked done, it is read and
    checked again on every pull, `rodu sync` lists it as waiting, and it lands when the compacted
    file arrives;
  - stopped between write and removals: the old files hold nothing new, and a later compaction
    removes them.

  Removals first with no later file leave nothing to wait on, so a reader also keeps, per replica,
  the highest number among the files it has dealt with. A replica's numbers only grow, and a
  compacted file is numbered above every file it replaces. When none of that replica's files
  present is numbered that high, its files were removed and the compacted file has not arrived:
  - every pull says so (`rodu sync` and the warnings of other commands);
  - the mark stays until a file numbered that high or higher is there, which resolves it;
  - a reader cannot tell a file still in transit from one that will never come, so it keeps saying
    so until the file arrives.

  Only files that landed raise the mark, so a damaged or refused file planted under a high number
  raises nothing. Someone who can write the folder can still rename a file that lands to a high
  number and remove it later; teammates are then warned until that replica's numbers pass it. They
  could as well delete files, which is worse. The same message appears if a replica's folder is
  emptied by hand.
- **Commands.** `rodu team create --folder <path> --no-encrypt` turns the workspace in place
  into a team workspace: the document is built from every row of its index, so events,
  idempotency records and versions stay. It prints an invite code, `rodu1-<workspace id>`, which
  holds no secret yet. The folder is checked before the workspace changes: one that already names
  a team is refused, unless its only replica folder is this workspace's own, left by a create
  that stopped half way (the creator's replica folder is made before the team file for this).
  `rodu team join <code> --folder <path> --name <you>` makes a workspace from
  the folder; a join that fails removes what it made. `rodu team` shows the folder, code and role;
  `rodu sync` syncs by hand. Giving neither `--encrypt` nor `--no-encrypt` is an error, so
  there is no silent default.
- **Automatic sync.** In a team workspace `add`, `ls`, `show` and `mv` read the folder first and
  write to it after; the numbering peer (the machine that created the team, until the role is
  handed over) numbers new cards in between. `web` and `mcp` sync when they start and when they stop. A folder that cannot be read
  is a warning, and the command, `rodu sync` included, works on what the machine has.
- **Live sync in `web` and `mcp` (built 2026-10-09, step 4c).** While they run, both servers
  keep syncing through a `LiveSync` seam in `rodu-core`, so `rodu-http` and `rodu-mcp` do not
  depend on `rodu-sync`. The servers call it under their own service lock, on the blocking pool,
  so sync work is serialized with requests and tool calls. Problems never fail a request: they
  go to stderr (never stdout, which is MCP's channel), and each message is written once until it
  changes, so an unreachable folder is not reported every few seconds.
  - `rodu web` pulls (and numbers, on the numbering peer) and pushes every 2 seconds, and pushes
    right after each write. A revision counter rises with every pull that took something in and
    every write; `GET /api/revision` returns it, behind the same token and host checks as the rest
    of the API.
  - The board asks for the revision every 3 seconds while the page is visible and no card is being
    dragged, and reloads when it changed. It reads the revision before the board on each load, so
    a change landing in between is caught on the next look. A teammate's card shows up within
    about 3 to 5 seconds.
  - The item panel is not reloaded, so an edit in progress is never overwritten; saving a card a
    teammate changed meanwhile is refused by its version, as before.
  - `rodu mcp` pulls before each tool call and pushes after it, so an agent reads teammates'
    latest changes and its own go out at once.
- **Handing over numbering (built 2026-10-10, step 4c).** The team document names the numbering
  peer: a root map `team` holds `numbering_peer`, 16 lowercase hex digits. `rodu team create`
  writes the creator's peer; anything else in that field, from a broken or hostile machine, counts
  as no peer. After every pull (each command, `rodu sync`, and each tick of a running `web` or
  `mcp`) a machine numbers cards only if the document names it, and records its role in its
  config so the next command starts right. A team made before the document named one keeps the
  config's choice: its creator names itself at its next sync that reaches the folder, never
  offline, so a take-over already in the folder arrives before it could be undone.
  - `rodu team take-numbering --yes` makes this machine the numbering peer, for when the one that
    numbered is gone for good: it pulls, names itself, numbers waiting cards and pushes. On the
    machine that already numbers it writes nothing. Without `--yes` it refuses, since two machines numbering at once give out the same numbers. `rodu team`
    shows which machine numbers.
  - The old machine, once it syncs, says once that numbering moved and makes provisional cards
    from then on. Cards it numbered while offline after the hand-over can clash with the new
    machine's: the clash rule above keeps the number on the card with the lowest id, and the new
    numbering peer numbers the other again. So a real number can change once in that case, the
    one exception to "a real number never changes", and only after a hand-over.
  - Two machines taking over at once, or an old-format team's creator naming itself while a
    take-over is still on its way, are settled by the document: one value wins on every replica,
    and the other machine stops numbering when it syncs.
- **The readable copy (built 2026-10-10, step 4c).** A team can keep a plain Markdown copy of
  its board in `<folder>/readable/`, for reading from Drive or a phone: one `<KEY>.md` per card
  (fields, description, links, comments) and an `index.md` by collection and status. It is a
  setting in the team document (`team`.`readable_copy`; only `true` is on), turned on with
  `rodu team create --readable-copy` or `rodu team readable-copy on` by any member, and off with
  `off`. Turning it on always warns that the copy is never encrypted, also for an encrypted team.
  - Only the numbering machine writes it, so folder apps never see two writers. It renders after
    each pull, write and live tick, but skips the work while the document version it last
    rendered from is unchanged, and rewrites only files whose content changed (temp file, then
    rename).
  - File names come only from card keys filtered to ASCII letters, digits and `-`, so card text
    never chooses where a file goes. The copy's folder is made only inside a team folder that is
    there; an unmounted folder is never recreated.
  - Rodu overwrites or deletes only what it wrote: `readable/.rodu-readable.json` records each
    file and the SHA-256 of what was written, and names in it must be ones Rodu could have
    written. A file that no longer matches (edited by a person) is left alone with a warning, and
    other files in the folder are never touched. Turning the copy off removes Rodu's files and
    the folder if it is then empty.
  - Problems writing the copy are warnings; the pull never reads `readable/`. Changes made to the
    copy are not read back.
- **Left for later steps.** Live sync polls on a timer rather than watching the file system, and an open
  item panel shows a teammate's change only when it is opened again. A file waiting on operations that never arrive
  (its predecessor refused, or never written) is read and checked again on every command, with no
  limit yet; `rodu sync` lists such files. Held files count toward the import cap of each later
  check, so a replica that writes close to 64 MiB of files that never land can hold back what
  others write until they are removed. After a replica compacts, each teammate reads and checks
  its whole history once, as one file. A compacted file holding an operation teammates once
  refused is refused whole by anyone who joins later, so that replica's other operations reach
  them only through its later files. A frame's length is not authenticated: a length past
  what any sync file holds marks the file damaged, but one changed to a larger length still within
  that bound cannot be told from a file still arriving, so it is listed as arriving for good (and
  never imported). A join that fails after its principal reached
  the folder leaves that principal behind; joining again under the same name then shows `bob2`.

## Encrypted teams (built 2026-10-09)

Step 4b: `rodu team create --folder <path> --encrypt` seals every sync file, so the folder's
provider cannot read the board.

- **Cipher.** XChaCha20-Poly1305 from RustCrypto's `chacha20poly1305` 0.11.0 (Apache-2.0 OR
  MIT), with a 256-bit team key and a fresh random 24-byte nonce for each file. Random nonces are
  safe at any number of files with a 192-bit nonce; if the random source fails, nothing is
  written.
- **Wire format.** A sealed file is framed like a plain one, under the magic `RODU-SEALED1`:
  magic, the sealed payload's length (u64 LE), its SHA-256, then `nonce || ciphertext || tag`.
  The frame still tells a file being synced from a damaged one before anything is decrypted. The
  associated data is `"rodu-sync-seal-1" 0x00`, the workspace id's byte length (u64 LE), the
  workspace id as written in the team file (UTF-8), and the writer's peer id (u64 LE). So a file
  moved into another replica's folder, copied from another team, or changed by one bit does not
  open; it is reported once, like a damaged file. A unit test pins a test vector computed by an
  independent implementation written from RFC 8439 and the XChaCha draft.
- **Team file.** Format 2: `{"format": 2, "workspaceId", "encryption": "xchacha20poly1305",
  "keyCheck"}`, where `keyCheck` is the hex SHA-256 of `"rodu-team-key-check-1" 0x00 || key`.
  Plain teams stay format 1, and a rodu that knows only format 1 refuses a format 2 folder.
- **Which side decides.** The workspace, never the folder: `config.json` records
  `"encrypted": true` and the key is in `.rodu/team.key`. The folder must match, or nothing is
  synced and the command warns:
  - an encrypted workspace and a format 1 folder (a team file turned back to plain);
  - a key whose check does not match;
  - a team file naming another workspace id than the config's. Files are sealed for the
    config's id, never the folder's, so a team file with only its id changed cannot make
    teammates write files nobody can open later;
  - a plain workspace and a format 2 folder;
  - an encrypted workspace without its key file, or a plain one with a key file.

  A provider that rewrites the team file can stop the sync, but it can never make a workspace
  write plain files. Create writes the key first, then the document, the team file and the first
  file, and the config last; a create that stopped half way carries on with the key it left, and
  a plain create there is refused.
- **Key.** It is stored as hex in `.rodu/team.key`, created with mode 0600 on Unix, never in
  `config.json`, and held in memory in zeroizing buffers. The invite code of an encrypted team is
  `rodu1-<workspace id>.<key, 64 hex>`. `team create` prints it once, with a warning that it is a
  secret. `rodu team` hides it unless `--show-invite` is given. `rodu team join -` reads it from
  stdin, so it stays out of shell history and other users' process lists. Join checks the key
  against `keyCheck` before it writes anything. A wrong key, a missing key, or a key on a plain
  team's code is refused.
- **Untrusted input.** A sealed file is opened, and so authenticated, before its plaintext goes
  through the same child-process import check as a plain file's. Files are read with a bound, so
  one that grows after its size was checked is refused like an oversize one. Encryption keeps
  the provider out, not a teammate: anyone with the invite code can write files the team
  accepts. It protects what the board says, not whether it syncs: whoever can write to the
  folder can still delete files, put back old ones (harmless, since operations already held
  are ignored) or stop the sync, and that is not detected.
- **Left for later.** Turning encryption on or off for an existing team,
  and changing the key, which needs a new team today. On Windows, `team.key` relies on the
  user profile's permissions, since there is no 0600.

## Proposed design for the open problems

1. **Card numbers (decided 2026-10-09: provisional keys).** A new card gets a provisional key
   built from the random bits of its id, six letters such as `DEMO-KQMRTZ` (no digits, so it never
   looks like a real number; no I, L, O or U; growing to 8 or 10 letters if the key is taken), and
   keeps it until it is numbered. Letters only, because `~` and the other symbols are JQL-lite
   operators. Exactly one peer per
   workspace, the *numbering peer*, gives out real numbers: when it imports a provisional card, it
   assigns the next number and writes it to the card. Two peers numbering at once would collide
   again without a server, so no other peer ever numbers. The numbering peer is the one that ran
   `init`. A command can hand the role to another peer if that machine is gone for good (built:
   `rodu team take-numbering`, under "The team folder"). A real
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
   update files under `sync/<peer-id>/<sequence>.update`, and each peer
   periodically compacts its own files into one (built: see "The team folder"). Rodu never talks to those services; their own apps move the files. A self-hosted
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
   peer is the only peer and every card is numbered immediately, as today. Done, the hand-over
   command in step 4c: `Item.number` is optional, `RoduService::with_numbering` and `assign_numbers` exist,
   and SQLite schema 2 migrates schema 1 workspaces in place.
3. Build a Loro-backed store implementing `Store`, keeping the SQLite index in sync from the
   document's change events, and run the existing service tests against it. Done: `LoroStore`
   in `rodu-sync`, described under "The Loro store" above. The CLI still opens plain workspaces
   with `SqliteStore`; step 4 opens team workspaces with `LoroStore`.
4. Shared-folder transport with automatic sync, `rodu team create` and `rodu team join`, and
   the encryption and readable-copy options. Test it with two and three workspaces on one folder
   in CI on macOS and Windows, including a folder app's conflict copies (`file (1).update`) and
   half-written files. Split in three: 4a (transport, team commands, automatic sync) and 4b
   (encryption) are done, described under "The team folder" and "Encrypted teams" above; 4c
   adds the readable copy, live watching in `web` and `mcp`, compaction and the numbering
   hand-over. Compaction is done, with the `stat` skip, and so are live sync in `web` and `mcp`
   the numbering hand-over and the readable copy, all under "The team folder". Step 4 is done.
5. Later: `rodu relay` for live sync.
6. Move to the Loro release that replaces `im` with `imbl` once loro-dev/loro#1122 lands, and
   drop the advisory exceptions.

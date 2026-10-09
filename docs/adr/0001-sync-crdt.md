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

## Proposed design for the open problems

1. **Card numbers (decided 2026-10-09: provisional keys).** A new card gets a provisional key
   built from its id, such as `DEMO-~a3f9`, and keeps it until it is numbered. Exactly one peer per
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
   peer is the only peer and every card is numbered immediately, as today.
3. Build a Loro-backed store implementing `Store`, keeping the SQLite index in sync from the
   document's change events, and run the existing service tests against it.
4. Shared-folder transport with automatic sync, `rodu team create` and `rodu team join`, and
   the encryption and readable-copy options. Test it with two and three workspaces on one folder
   in CI on macOS and Windows, including a folder app's conflict copies (`file (1).update`) and
   half-written files.
5. Later: `rodu relay` for live sync.
6. Move to the Loro release that replaces `im` with `imbl` once loro-dev/loro#1122 lands, and
   drop the advisory exceptions.

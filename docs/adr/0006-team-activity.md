# ADR 0006: A card's activity across the team

- Status: proposed 2026-10-29

## Context

Rodu 0.9.0 shows a card's activity: who changed which field, who commented and linked, and through
which agent (`rodu show <key> --activity`, the MCP tool `get_activity`, the Activity section of the
board's card panel). It reads the `events` table, which ADR 0001 keeps on each replica: "Syncing
every event would make the document grow with every edit forever." So in a team workspace it lists
only what was done on this machine. The CLI, MCP and the board all say so: "Recorded on this
machine: changes made on a teammate's machine are not listed."

The question people actually ask is about the team: who moved this card to Done, and when; did a
person or their agent change the description. An agent asked to explain a card's history gets only
the part that happened on its own machine.

What the document already has:

- **Who wrote each operation.** Every Loro operation carries its writer's peer id. The folder
  refuses a file holding operations of any peer but the one its folder names (ADR 0001). On a
  signed team every file is signed by a key admitted for that peer (ADR 0002, step 2). The
  `members` map ties each peer to a person.
- **Comments say who wrote them.** A comment's `author_id` and `via_agent_id` are synced fields.
- **What changed.** Each change's operations hold the values it set.
- **Not when, and not one change per write.** Rodu does not turn on Loro's timestamps, so every
  change has time 0. Loro then merges consecutive changes from one peer, so one change can hold many
  writes to many cards (measured below: 120,000 writes became 187 changes).
- **Not the agent.** Agents write through their owner's replica (ADR 0001, item 9), so an agent's
  write and a person's write by the same machine look alike.

## Options

Measured with Loro 1.16.2 (the version Rodu ships), release build, Apple M4. A board was built
from 20,000 cards and then 100,000 edits (a title or a status), one commit per write as
`LoroStore` makes them. The figures are the snapshot (`rodu.loro`), all updates (what a compacted
sync file holds), and the time to export every change's operations as JSON. That export is only
part of indexing: writing the rows and finding each operation's card are not measured yet (step 4).

| | snapshot | all updates | export the whole history |
|---|---|---|---|
| today: no timestamps, changes merged | 2.79 MB | 5.52 MB | 13 ms (187 changes) |
| Loro timestamps, one change per write | 2.84 MB (+2%) | 5.68 MB (+3%) | 28 ms (120,000 changes) |
| same, plus the agent's id on 1 write in 3 | 2.86 MB (+2.5%) | 7.28 MB (+32%) | 33 ms |
| no Loro timestamps; the writer's time as a message on every write | 3.36 MB (+20%) | 7.83 MB (+42%) | 39 ms |
| a full event on every write (request, actor, agent, action, card) | 4.26 MB (+53%) | 29.3 MB (×5.3) | 44 ms |

At 2,000 cards and 22,000 writes the ratios are the same (0.80 MB of updates today, 0.83 MB with
timestamps).

1. **Sync the events table into the document.** Each event's before and after values would live in
   the document's state as well as its history, for ever. Any member can overwrite or delete a map
   entry, and an event's actor is whatever its writer put there, not who wrote it. Rejected.
2. **Each machine writes its activity to a file in its replica folder,** like `status.json`
   (ADR 0005). The file grows without end, a machine joining later needs every machine's whole file,
   and anyone who can write the folder can rewrite it. It would repeat what the operations already
   say. Rejected.
3. **A full event as each change's commit message.** Loro keeps a message per change, in the
   writer's own signed operations, so nobody else can change it. But it repeats what the operations
   already hold and still proves nothing beyond them. It also makes compacted sync files 5.3 times
   larger. Rejected.
4. **The writer's time as a message on every write.** This would avoid Loro's clamp (see "When"
   below), but it costs 42% more in every compacted file. Reading Loro's time against the change's
   dependencies and against when this machine received it gets the same honesty for 3%. Rejected.
5. **Read activity from the document's history** (chosen). Turn on Loro's timestamps, keep one
   change per write, and name the agent in the commit message only on an agent's write. Each machine
   indexes the history as it arrives. Who wrote each change is then as proven as the operations
   themselves:
   - on a signed team, by the file's signature;
   - on an unsigned one, as much as the folder check proves, which is a claim.

## Decision

### Writing

The document is configured when `LoroStore` opens it, and in `team create` and `team join`:

- `set_record_timestamp(true)`. Every change gets a time in whole seconds since 1970.
- **Every write is a change of its own.** Loro merges a peer's change into the one before it when
  all of these hold:
  - it directly follows that change, with a single dependency on it;
  - their times are close enough;
  - their commit messages are equal.

  For a local change, "close enough" is the document's merge interval. For a change taken in from
  another machine, it is always the same second. Whether two changes actually merge also depends on
  that machine's storage: whether the block before is in memory, and whether it is full. So the
  same two writes could be one change on one machine and two on another. Every change therefore
  gets a message that differs from its peer's previous change, and none of them ever merges. The
  message starts with a parity digit, `0` or `1`, the opposite of the previous change's (`1` after
  a change with no message).

  *As built:* the ADR first chose a merge interval of `-1`. Testing showed that it does not reach
  changes taken in from another machine (`oplog/change_store.rs`, `insert_change_inner`). A review
  then showed that no interval makes every machine merge alike. At 20,000 cards and 120,000 writes
  the parity digit costs 2% of a compacted sync file (5.80 MB against 5.68 MB), and nothing in the
  snapshot.
- **Context for the commit.** The `Store` trait gains
  `fn set_commit_context(&self, at: i64, via_agent_id: Option<&str>)`. The service calls it at the
  start of each write transaction, from its own clock and the `Actor`. `SqliteStore` ignores it.
  - `LoroStore` keeps it until the outermost transaction ends, committed or not.
  - The outermost transaction is one change. The first context set in it names that change, and a
    service write nested in it (as MCP's `plan_cycle` nests several) does not take it over.
  - At `stage()` it applies the context:
    - `set_next_commit_timestamp(at)`, so tests with a fixed clock stay deterministic. Loro still
      lifts the time to that of the latest change it builds on (see "When"). A write with no
      context, such as recording a member or the numbering peer from the sync code, takes Loro's
      own clock: timestamps are on for every change.
    - The message: the parity digit, then ` via <agent principal id>` on an agent's write, or
      ` adopt` on the change that makes the team board from a workspace (see "Team create").
      Readers take these from the suffix and ignore any message in another form, so a later
      version can add fields.
- **Imports carry no context.** An import adds no change of this machine's own: Loro applies the
  teammate's changes as they were made.

Checked (`each_write_is_a_change_of_its_own_with_the_services_time_and_its_agent`). Writes made in
one second, some by another process after reopening the saved document, keep the same change
boundaries, times and messages:
- in a sync file;
- on another machine that took them in over two pulls, and in what that machine passes on;
- in the saved snapshot, and after reopening it.

With the parity digit taken out, that test fails: three writes come out as one change.

Loro's update format does not change, so rodu 0.9 reads these files as before.

A batch is one change. Examples: one `stage()` that numbers many cards, or `team create` building
the document from the index. Its activity is one entry per card it touched, all at one time.

### Indexing

Each replica keeps a table in `rodu.db`, `team_activity`, rebuilt from the document like the rest
of the index. There is one row per operation that touches a card:

```text
peer, counter, lamport, at, deps_at, parity, via_agent_id, card_id, action, field, value
```

- `counter` and `lamport` are the operation's own, not its change's.
- `at` is the change's Loro time.
- `deps_at` is the greatest time among the changes it depends on (0 when it has none).
- `parity` and `via_agent_id` are what the change's message says, or null for any other message.
- `action` uses the names `events` already uses, so `ActivityLog` and its formatting keep working:
  - `item.create`: the card's tree node and its first fields;
  - `item.update`: one field of the card's map, with the value set, or its parent;
  - `item.number`: a later `number` on the card (reading shows it only from the numbering peer);
  - `comment.create`: a comment entry whose `item_id` is the card;
  - `link.create`: a link entry whose `from_item_id` is the card.

A second table, `change_received`, is local data, so a rebuild keeps it, as it keeps `events`. It
records when this machine first held each change, by peer and counter range, from this machine's
clock:
- on import: when the change landed (not when its file arrived, since a held file can wait);
- for its own write: the write's time;
- on join, or the first open after upgrading: the time of that join or open, for everything
  already held.

- **On import.** After `import_batch` lands files, the store turns the version it held before and
  the one it holds now into version vectors (`frontiers_to_vv`). It reads the changes in between
  with `export_json_updates`, maps each operation to its card and adds rows. Operations that are
  still pending are not in the log yet. They are indexed by the import that finally lands them.
  A local write indexes its own change the same way, right after `stage()`.
- **On a rebuild,** the table is rebuilt from the whole history. Its rows equal the ones kept up
  to date: a row depends only on its change, read whole as the document stores it. Loro may join a
  peer's consecutive changes that have no parity digit on import, and then hands the later one to
  an incremental read as a slice; its `deps_at` and whether it made the card come from the whole
  change, not the slice. A comment or link is read from the fields its own change gives it, and a
  card's fields by the tree node whose id their map has, deleted or not. A saved version the table
  cannot read back starts it over from the whole history instead of stopping writes.
- **Known limit:** a row names the card id its node holds now. Rodu never changes it; a peer
  outside Rodu that rewrites it moves the node's rows to the new id on the next rebuild.
  Likewise, a comment or link that such a peer splits over two changes Loro then joins gets its
  row only on a rebuild, since each half alone is not a whole entry.
- **Values are untrusted,** like everything indexed from the document (ADR 0001):
  - a value goes into the table only if it passes the same check the card's field would;
  - a field Rodu does not know is left out;
  - the description and the rank are recorded as "edited" without their value, as in 0.9.0.
- **Shallow snapshots.** Reading activity from history rules out Loro's shallow snapshots, which
  ADR 0001 counted on to keep a long-lived board from growing. A replica could keep the rows it has
  already indexed, but a machine joining after one would start its activity there. If shallow
  snapshots are ever needed, that is the cost, and this ADR is revisited then.

### Reading

`RoduService::activity` reads `team_activity` in a team workspace and `events` in a workspace that
syncs with nobody, as in 0.9.0. Both give the same `ActivityLog`.

- **Order.** Entries are listed by Lamport timestamp, then peer: an order that never puts a change
  before one it built on. They are not sorted by time.
- **Before and after.** The value an entry replaced is the value of the latest entry for that card
  and field that its change depends on. That entry is found through the change's version vector,
  not by taking whichever entry comes before it in the list. So when two machines set a field at the
  same time, both show the same earlier value, and the document keeps one of the two (ADR 0001).
- **Who.** The writer's peer, through `members`, gives the person's name.
  - A peer no person claims, and on a signed team a peer that is not admitted, is shown as
    `unknown machine 1a2b3c4d`, as `rodu team members` shows it.
  - Who is admitted and where each removal cuts are the team folder's to say, and activity is read
    without the folder: every pull keeps what the folder says then in local data (the `trust`
    note), and the reader goes by that. Every command pulls first.
  - On an unsigned team the name is what the machine says. The note under the activity says so, as
    `rodu team members` does.
  - **A removed machine.** Its entries are shown with `(removed)`. The cut counts operations, and a
    change can straddle it, so the check is per row, against the cut as it is when read. The cut
    can move (ADR 0002, 3a). Rows past the cut can exist, because they were taken in before the
    removal was known (ADR 0002, 2b). They are shown, marked `(after removal)`: work taken in is
    never taken out.
  - **A comment** is attributed to the peer that wrote it, like any other entry. If the comment's
    own `author_id` names someone else, the entry says so ("ann, as bob").
- **Through which agent.** `via <id>` counts only when the id is an agent principal owned by that
  peer's person. A machine cannot put its writes on someone else's agent. Any other message is
  dropped and the entry stands without it. Ownership is read from the principals as they are now.
  Any member can rewrite those, so the agent label is as good as that map; the person is still the
  peer's.
- **When.** Loro never gives a change a time earlier than the changes it depends on: its time is
  the larger of its writer's clock and `deps_at`. So a time can be inherited:
  - from a teammate whose clock runs ahead;
  - by a rodu 0.9 machine's change, whose own time is 0, once it builds on a timed change.

  The reader takes this into account, along with when this machine received the change (`received`):

  | the change's time | shown as |
  |---|---|
  | 0 | `earlier`: no time recorded (rodu 0.9 or older, or before this ADR) |
  | later than `received` | `before <received>`: the writer's clock runs ahead |
  | equal to `deps_at` | `after <time>`: the time may be inherited |
  | otherwise | the time, as the writer's clock gave it |

  Ahead-of-time clocks then never push an entry past the moment this machine had it, whoever's
  clock it was.

  This machine's own changes are read by what it knows of them: each takes this machine's clock to
  the second, sync writes included, and is noted as received then. So one is shown at its time,
  or as `after` it when Loro lifted it to a change it built on, never `before`. A teammate's
  change made in the same second as one it built on reads `after`, since this machine cannot tell
  the two apart.

  *As built:* a sync write (the numbering, a member's record, `adopt`) first took Loro's own clock,
  which rounds to the nearest second, half the time past the second this machine noted. Such a
  change then read `before`, as if this machine's clock ran ahead.
- **Changes without a parity digit** come from rodu 0.9 or older, or from before this ADR. How
  many such changes Loro made of their writes differs between machines: it depends on each
  machine's storage when it took them in (see "Writing"). So the reader never treats them one by
  one. A peer's run of consecutive changes without a parity digit is shown as one entry per card,
  listing the last value of each field it set, with the time of the run's first change (or
  `earlier`). A merged change keeps the time of its first write, so that time is the same on every
  machine, and every machine shows the same entries for them.
- **Team create.** The change that builds the team document from a workspace (`adopt`) says so in
  its message, so every machine knows it. The creator also notes, in local data, the time of the last
  event it had recorded then: its events up to that time come first, and a write made right
  after, even in the same second, is read from the document only. Its entries read "added to the team board", not
  "created". On the creator's machine, the card's `events` from before it come first, since they
  are this machine's own and are kept, and they hold the card's comments and links, so the adopt
  change's own entries for those are left out there.

  *As built:* the ADR first kept the adopt change's id in the creator's local data. That would
  have marked it on the creator's machine only.
- **Removed machines, per row.** A change can straddle a cut, so its part past the cut is an entry
  of its own, marked `(after removal)`.
- **Known limits.** Where a run of changes without a parity digit starts is found again on each
  read, by walking that peer's history back; for a team with a long Rodu 0.9 history this is the
  slow part, and the place to keep it in the index if it shows in step 4. The person comes from
  `members`, which ADR 0002 lets any admitted member write: a member can claim another person for
  a machine, as `rodu team members` would show it.
- **This machine's events still exist.** `events` keeps request ids and exact before and after
  values for audit. In a team workspace, `rodu show --activity --local` lists them as 0.9.0 does.

The note under the activity changes from "Recorded on this machine" to:
- on a signed team: "Times are each machine's own clock";
- on an unsigned team, also: "names are what each machine says".

### Steps

1. **Writing:** `set_commit_context`, timestamps, no merging, the agent message. Tests:
   - a write is one change with the service's time;
   - an agent write carries `via`, and a person's write carries no message;
   - a rollback leaves no context behind;
   - all of it survives a sync file, a compacted file and a reopened snapshot.
2. **Indexing:** `team_activity` and `change_received`, filled on import, on local writes and on
   rebuild. Shared service cases on the Loro store check:
   - a field change, a comment, a link and a creation, from a second replica;
   - a file held for missing operations is indexed when they land;
   - a rebuild equals the incremental table and keeps `change_received`, including after changes
     from outside Rodu that Loro joins on import or that delete a card's node;
   - an unreadable saved version starts the table over;
   - a move, an agent's write, the parity and a long comment cut to 500 characters.
3. **Reading:** the order, the before value, the time rules, `--local` and the audit, in two pull
   requests: reading (3a), then the audit (3b). Tests:
   - two concurrent writes to one field show the same earlier value;
   - a 0.9-style change shows as `earlier`, and as `after` once it builds on a timed change;
   - a clock two hours ahead shows as `before`;
   - on a signed team: an unadmitted peer as an unknown machine, a removed machine's entries before
     and after its cut, and a `via` naming another person's agent dropped;
   - the adopt change reads as "added to the team board";
   - `item.number` from a peer other than the numbering peer is not shown as a numbering;
   - a comment or link row whose entry the board's index does not hold is not shown;
   - `rodu audit` and `get_audit` show the machine, and are the same after `team_activity` is
     edited by hand;
   - an untimed change after a timed one from the same peer is flagged;
   - the browser smoke test checks a teammate's change in the panel.
4. **Measure again** at 20,000 cards: a compacted file's size, open, rebuild with its rows, and
   `show --activity`. Record the figures here.

   *Measured (2026-10-30):* release build, Loro 1.16.2, Apple M4, with the ignored test
   `measure_team_activity_on_a_large_board` (`crates/rodu-sync/tests/store.rs`). It builds a board of
   20,000 cards, then makes 100,000 edits (a title or a priority), 1,000 of them to one card. The edits
   run 1,000 to a transaction to keep the run to minutes, so the document holds fewer changes than one
   per write. The operations, and so the rows, are the same.

   | | |
   |---|---|
   | snapshot (`rodu.loro`) | 6.9 MB |
   | all updates (a compacted sync file) | 12.6 MB |
   | `team_activity` | 372,170 rows, 77.5 MB with its two indexes (about 210 bytes a row) |
   | `rodu.db` in all | 303 MB, of which `events` is 194 MB (120,002 events) |
   | one card edit through the service | 56 ms |
   | open the store | 0.18 s |
   | rebuild the index with its rows | 4.3 s |
   | import a teammate's file of 1,000 edits (one change each) | 3.0 s |
   | `show --activity`, a card with 6 entries | 0.6 ms |
   | `show --activity`, a card with 201 entries | 3.9 ms |
   | `rodu audit`, either card | 0.43 s |

   What it shows:
   - Reading a card's activity is cheap: it reads the card's rows through their index.
   - The audit reads the whole history each time, as designed. At 0.43 s for a command run now and
     then, it stays that way (the `shortcut:` in `audit` names this measurement).
   - `team_activity` is the largest thing this ADR adds: over 10 times the snapshot. Most of it is
     the card creations (270,297 rows, 73%; the edits are 101,826), one row per field of each new
     card. That growth follows what the ADR
     settled: the table is not capped.
   - The local `events` table, which is older than this ADR, is larger still, because it keeps every
     write's before and after values (about 1.1 KB an event). It is outside this ADR, and is noted here as a separate thing
     to look at.
   - The rebuild (4.3 s, against 0.7 s without rows in ADR 0001) and the import (3 ms a change) are
     where indexing activity costs time. Both run rarely, or in the background of a sync.

## What this does not do

- **Prove when.** A time is what clocks said: the writer's, or one inherited from a change it built
  on. The rules above keep a clock that runs ahead from showing times that have not happened yet.
  They cannot catch a clock that runs behind, or a member who backdates their own changes. The
  order of entries does not depend on time.
- **Prove the agent.** `via` is the writing machine's word that its owner's agent made the write.
  It cannot be checked any further than that machine.
- **Undo or hide history.** Activity reads what the document holds. A removed member's work stays,
  attributed (ADR 0002).
- **Activity for anything but cards.** Changes to people, collections, cycles, members and team
  settings are not indexed. `rodu team members` and `rodu team` cover the parts people ask about.
- **Give history to workspaces made before this.** Their changes have no times, so they show as
  `earlier`, merged.

## Privacy

Every member, and an unencrypted team's folder provider, can now see when each person edits, to the
second. Of the four:
- an encrypted team's provider sees only the sealed files, whose own modification times already
  show when each machine pushed;
- an unencrypted team's provider can already read the board.

What is new is that members see it per card, per field. That is the point of the feature, and it
is the same as what any shared tracker shows.

## Settled (2026-10-29, by the owner)

1. **Timestamps are always on.** There is no setting to turn them off, for a machine or a team.
   Rodu cannot stop a modified build from writing changes without a time. So the audit (below)
   flags a change with time 0 from a peer that wrote timed changes before it: Rodu itself never
   writes one.
2. **`team_activity` is not capped.** It grows by one row per field change, like the history it is
   built from. Capping a table that a rebuild fills again from the uncapped history would save
   little. Its size at 20,000 cards is measured in step 4.
3. **The machine is kept for audit, not shown.** The board, `show --activity` and `get_activity`
   show the person and the agent. They do not show the machine.

### Audit

`rodu audit <key>` and the MCP tool `get_audit` list a card's entries with everything that is
known about each one:
- the machine (peer id, 16 hex digits);
- the change id (peer and counter) and its Lamport timestamp;
- the raw time, `deps_at` and `received`;
- the commit message;
- whether the writer is admitted, removed, or past its cut.

It is read-only, and there is no way to edit it:
- nothing in Rodu (CLI, MCP or the local API) writes activity;
- the audit does not read `team_activity`, which anyone with access to the machine could edit in
  `rodu.db`. It reads the changes from the document itself, each time it runs. The document's
  history is only ever added to. On a signed team, each change is signed by the machine that wrote
  it.

What it proves follows from that:
- On a signed team it proves which admitted machine wrote each change.
- On an unsigned team the machine is what the files say. The audit's output says which kind of
  team it is.
- On either, a time is what the clocks said (see "When").

Any member can run it. Every member holds the whole document (ADR 0002), so limiting the command to
the owner and admins would only hide what each member's own copy already holds.

The audit is step 3.

*As built (3b):*
- The audit lists the document's changes, not entries: one per change that touched the card, with
  each of its operations on the card (values as stored, ids included; a description or position
  says only that it changed). A change is not grouped into a run or split at a cut. An operation
  past a removed machine's cut is marked as such. The header names the card's id as well as its
  key, since the key is looked up in this machine's index.
- It lists the operations Rodu reads as touching a card, the same ones activity is built from.
  Operations Rodu ignores are not listed: a deleted key, an unknown key, a value a card's field
  would refuse, a deleted node, or a rewrite of a node's card id (which moves the node's history
  to the id it now holds, see `card_of`).
- Members come from the document; names come from this machine's index. Whether the team is
  signed, and each machine's standing, come from the note the last pull or push wrote, as
  activity's do (see "Who"). Anyone with access to the machine could edit that note, so the
  output says where the standing comes from; `rodu audit` pulls first, which writes it again from
  the folder when the folder can be reached. Before the first pull or push, the audit says the
  team's standing is not known yet. `received` is this machine's own note.
- The untimed flag is worked out per machine in counter order, whatever order Lamport times put
  the changes in. It cannot see an untimed change that Loro joined, on import, to the timed change
  before it: the joined change has the earlier change's time.
- It reads the whole history on every run. Step 4 measures that at 20,000 cards.
- `rodu audit` syncs first, as every command does, so it may number cards or record this machine
  as a member before it reads. A plain workspace has no team document, so the audit refuses to
  run there.

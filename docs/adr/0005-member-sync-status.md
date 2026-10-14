# ADR 0005: Show when each machine last synced

- Status: accepted 2026-10-14

## Context

A team of three had joined through Google Drive. The owner asked whether the two new machines
were still syncing, and nothing in Rodu could tell. The only signs in the folder were each
machine's numbered files, and a machine writes those only when it changes something. A machine
that reads the board every day and never edits a card looks the same as one whose Google Drive
stopped syncing, or one that was switched off for good.

`rodu team members` and the board's Team panel list each person's machines. They should also say
when each machine last synced, whether it had everything this machine had, and which Rodu
version it runs, so an owner can tell "has not changed anything" from "is not getting anything".

## Decision

Each machine keeps a small status file in its own replica folder: `status.json`, or
`status.sealed` for an encrypted team.

```json
{"format": 1, "at": 1792000000, "holds": "<peer, 16 hex>:<end>,...", "version": "0.7.1",
 "publicKey": "<64 hex>", "signature": "<128 hex>"}
```

- `at` is the writer's clock, in seconds since 1970, when it last synced.
- `holds` is how much of each machine's operations the writer's document held then (its Loro
  version vector), sorted by peer.
- `version` is the writer's Rodu version.
- On a signed team, `publicKey` and `signature` sign it all:

  ```text
  "rodu-status-1" 0x00 || u64 LE byte length of the workspace id || the workspace id
      || the writer's peer id, u64 LE || at, u64 LE || u64 LE byte length of holds || holds
      || u64 LE byte length of version || version
  ```

  A team that does not sign leaves both out, and its status files are only what each machine
  says, like the rest of that team's folder.

### Writing

At the end of each pull (every command, `rodu sync`, and each tick of a running `web` or `mcp`),
a machine rewrites its status file when:

- there is none of its own that reads back, or
- what it holds or its version changed, and it last wrote at least 60 seconds ago, or
- it last wrote 10 minutes ago or more, or its clock is now before what it last wrote.

So a running board rewrites the file at most once a minute while work comes in and once every 10
minutes when nothing changes, and a machine running a command now and then writes once per
command at most. The file is written like every other file in the folder: under a hidden
temporary name, then renamed over the old one. A problem writing it is a note on the pull, like
the seen and exchange files, never a failed sync.

### Reading

`rodu team members` and the Team panel read the status file of each machine they list, other than
this one. It is untrusted, like everything in the folder:

- a link, a file over 64 KiB, one that does not parse or open, or one with a field out of shape is
  no status;
- `holds` takes at most 1,024 entries, each a peer of 16 lowercase hex digits and a decimal end;
  `version` takes at most 32 characters among ASCII letters, digits, `.`, `+` and `-`;
- on a signed team the status counts only if its signature verifies for the folder's peer and its
  signer is admitted for that peer or may admit, as for seen files. A file copied into another
  machine's folder does not verify there.

A machine's line then reads, for example:

```text
  machine 1a2b3c4d — synced 4 min ago, up to date with this machine, rodu 0.7.1
  machine 5e6f7a8b — synced 2 days ago, behind this machine, rodu 0.7.1
  machine 0c0ffee0 — no sync status yet (an older rodu, or it has not synced since it joined)
```

"Up to date with this machine" means that for every machine whose work this one holds now, the
writer held at least as much when it last synced; a removed machine's work counts only up to its
cut, since the team takes in nothing past it. "Behind this machine" means it lacked some of that.
Work made here or brought in since its last sync counts too, so right after an edit every other
machine reads "behind" until its next status goes out (a minute or so, plus the folder app's
delay). The age next to it tells the two cases apart: behind and synced a minute ago is work in
flight; behind and synced two days ago is a machine that stopped syncing.

How long ago is this machine's clock minus `at`: "just now" under a minute, then minutes, hours
and days. A time ahead of this machine's clock by up to five minutes reads "just now"; further
ahead, "synced at a time ahead of this machine's clock" rather than a negative age.

A machine writes its first status on the pull after its first push, never into a replica folder
that is not there yet: a join's first pull, which may still be refused, leaves the team folder as
it was.

## What this does not do

- It does not prove a machine read the folder. A machine can write any `at` and `holds` it likes;
  the signature proves only which machine said it. A member who wants to look active can, and so
  can the owner or an admin for any machine, since their keys count for every replica folder, as
  for seen files.
- Anyone who can write to the folder can put an older signed status back. That makes a machine
  look less recent, never more. Keeping the newest `at` seen of each machine would stop it; not
  worth a local note yet.
- After a re-key, a machine's status stays sealed with the key it was sealed with until its next
  rewrite, up to 10 minutes, so a machine just removed can still read when members last synced
  for that long.
- It does not show anything for machines on Rodu 0.7.0 or older: they write no status file, and
  read "no sync status yet".
- It tells teammates when each machine was in use, to the minute. That is the point of the
  feature, and it stays inside the team: the file is sealed for an encrypted team.
- No history: each machine keeps one file, rewritten in place.

## Considered

- **The time of each machine's newest numbered file.** It is already there, but it moves only
  when the machine writes, which is what failed here.
- **Keeping the status in the team document.** Every rewrite would be an operation every machine
  keeps forever, and it would wait on the operations before it.
- **File modification times as `at`.** Folder apps set their own times when they download a file,
  and a time inside the signed content cannot be shifted by the folder.

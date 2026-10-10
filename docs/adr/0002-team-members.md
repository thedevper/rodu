# ADR 0002: Who is on a team, and proving who wrote what

- Status: proposed 2026-10-10

## Context

A team workspace (ADR 0001) knows people only as principals in the team document: `team create`
and `team join` each add a human principal and an agent principal owned by it. Three things are
missing:

1. **Nothing lists the team.** There is no command that shows who has joined, which agents act for
   whom, or which machines belong to which person.
2. **A machine is not tied to a person.** Each replica writes under its own Loro peer id, and
   the folder check already refuses a file holding operations of any peer but the one its folder
   names. But nothing records which person a peer id belongs to, so `sync/3f9a…/` cannot be shown
   as "Ann's laptop".
3. **Anyone who can write to the folder can write as anyone.** ADR 0001 accepted this for a small
   trusted team and named per-principal signing as later hardening. A plain team's invite code is
   only the workspace id. Anyone with write access to the shared folder can add a replica folder,
   and that includes everyone it is shared with and the folder's provider. An encrypted team keeps
   the provider out, but not anyone holding the invite code. Nobody can be removed: a removed person
   keeps the key and can keep writing.

`rodu relay` (ADR 0001, step 5) should come after this, so a relay can refuse files from
non-members instead of forwarding whatever arrives.

## Decision

Three steps, each shippable alone.

### Step 1: a members list (no cryptography; built 2026-10-10)

- The team document gains a root map `members`, from peer id (16 lowercase hex, as `sync/` names
  it) to the human principal id that replica writes for. `team create` adds the creator's peer;
  `team join` adds the joiner's. Agents write through their owner's replica (ADR 0001, item 9),
  so they need no entry of their own.
- `team join --as <name>` joins a second machine for a person already on the team: it adds this
  peer under that existing principal instead of creating `bob2`.
- `rodu team members` lists each person with their agents and their machines (short peer id:
  the first 8 hex digits, which the replica folder's name starts with, or all 16 when two listed
  machines share them; marks this machine and the numbering machine). A replica folder whose
  peer is in no entry is listed as "unknown machine". The readable copy and `rodu team` do not
  change.
- This is a claim, not proof: anyone who can write to the folder can write an entry. The command
  says so until step 2 is in.
- As built: an entry whose key is not 16 lowercase hex digits, or whose value is not the id of a
  principal in the document, is ignored. A replica whose own entry is missing (a team made
  before this) or names someone else records itself after its next pull, once its person is in
  the document. `--as` refuses an unknown name or an agent's name, and the failed join leaves
  nothing behind. It uses that person's agent, or makes `<name>-agent` if they have none. A
  machine claimed for an agent is listed as unknown.

### Step 2: each machine signs what it writes

- **Keys per machine, not per person.** `team create` and `team join` generate an Ed25519 key
  pair. The private key is kept in `.rodu/identity.key` (mode 0600, zeroized in memory, like
  `team.key`), and the public key is recorded in the machine's `members` entry. A person on two
  machines has two keys under one principal, so a private key never has to be copied between
  machines.
- **Signed files.** Every sync file is signed over a domain tag, the workspace id, the writer's
  peer id, the file name and the payload's SHA-256. For an encrypted team the signature sits
  inside the sealed payload, so the provider cannot see who wrote what. A reader checks the
  signature against the key admitted for that folder's peer *before* the child-process import
  check.
  - A file from a peer with no admitted key is held as "waiting for admission", not imported.
  - A file whose signature does not verify is refused and reported once, like a damaged file.
  - Together with the existing one-peer-per-folder check, every operation in the document can then
    be tied to a person's machine.
- **Admission.** A joiner cannot admit themselves; that would prove nothing. `team join` writes a
  request (principal, peer, public key). The owner or an admin (see Roles) runs
  `rodu team admit <name>`, which signs an admission record with their own key. Every replica
  checks the chain of admissions back to a root key.
- **Root of trust.** The creator's public key fingerprint goes into the invite code
  (`rodu2-<workspace id>[.<team key>].<root fingerprint>`) and into each joiner's `config.json`.
  It never comes from the folder alone, because the team file can be rewritten by whoever writes
  the folder.
- **Migration.** An existing team keeps working unsigned. `rodu team sign` on the creator's
  machine turns signing on: from then on, unsigned files from any peer are held. Each existing
  member's machine signs a request on its next sync, and the admin admits them.

#### Step 2a as built (signed teams from creation; the creating machine admits)

Step 2 is built in two parts. 2a is below; 2b adds admins and the ownership transfer (Roles).
Where 2a differs from the text above, 2a is what holds:

- **Only new teams sign, for now.** `rodu team create --signed` makes a signed team; an
  existing team stays unsigned. `rodu team sign` (migration) comes later, and the second open
  question is settled for now as "a team can be signed from creation".
- **Only the creating machine admits.** Its public key is the team's root key. In 2a it is the
  only key that can sign an admission; admins (2b) will be keys the root admits as such.
- **Team file format 3.** `{"format": 3, "workspaceId", "signing": "ed25519", "root": <root
  public key, 64 hex>}`, plus `"encryption"` and `"keyCheck"` for an encrypted team. A rodu that
  knows only formats 1 and 2 refuses it rather than writing unsigned files. The workspace decides,
  as with encryption: `config.json` records the root key under `team.signing`, and a signed
  workspace refuses a folder whose team file is unsigned or names another root, and an unsigned
  workspace refuses a format 3 folder. So whoever writes the folder can stop the sync, but can
  never turn signing off or swap the root.
- **Invite code.** `rodu2-<workspace id>.<root public key, 64 hex>[.<team key, 64 hex>]`. The
  whole public key travels in the code, so a joiner never takes it from the folder.
- **Machine key.** Ed25519 (`ed25519-dalek` 3.0.0, BSD-3-Clause), 32 random bytes from the OS,
  stored as hex in `.rodu/identity.key` (0600 on Unix, zeroized in memory).
- **Signed payload.** Inside the frame, and inside the seal of an encrypted team, the payload is
  `0x01 || signer public key (32) || signature (64) || Loro update`. The signature covers
  `"rodu-sync-sign-1" 0x00 || workspace id length (u64 LE) || workspace id || writer peer id
  (u64 LE) || SHA-256 of the Loro update`. The file name is not signed: a folder app's conflict
  copy (`0000000003 (1).update`) must still verify, and a file renamed within its own replica
  folder holds operations readers already ignore by their ids. Moving a file to another replica
  folder, or to another team, fails the signature.
- **Who a file is accepted from.** A file signed by the root key is accepted from any replica
  folder: only the creator holds that key. Any other file is accepted only when the team document
  holds an admission for that folder's peer whose key is the signer's and whose signature verifies
  against the root key. A validly signed file whose signer is not admitted for its folder waits.
  It is not remembered as done, but its name, size and time are noted with its signer, so it is
  not read again until that signer is admitted for that folder. A file whose signature does not
  verify, or that is not a signed payload at all, is reported once as damaged. A pull that brings
  in a new admission reads the folder once more, so the admitted machine's files land in the same
  command.
- **Admission record.** The document's root map `admissions`, keyed `<peer id, 16 hex>.<member
  public key, 64 hex>`, each holding `<member public key>.<signature, 128 hex>`. The signature is
  the root key's over `"rodu-admit-1" 0x00 || workspace id length (u64 LE) || workspace id || peer
  id (u64 LE) || member public key`. Any member can write into the map, so a record counts only by
  its signature, never by who wrote it. Each record has its own entry, so admitting a machine
  never replaces another's record.
- **Admissions are kept once checked.** Any member can also overwrite or delete an entry. So each
  replica keeps, in its own index (never synced), every admission it has checked, and a later
  change to the document never shuts out that machine there. What remains: a member who deletes
  a record before some replica has read it keeps that replica, such as one joining later, from
  admitting the machine. That is a denial of service by a member, not a way in. Removing such a
  member is step 3.
- **Asking to join.** `team join` on a signed team writes `sync/<peer>/request.json`
  (`{"format": 1, "name", "publicKey", "signature"}`; sealed with the team key as
  `request.sealed` for an encrypted team, so the provider does not learn the name). The machine
  signs its own request over `"rodu-request-1" 0x00 || workspace id length (u64 LE) || workspace
  id || peer id (u64 LE) || name length (u64 LE) || name`, so nobody can file a request under
  another machine's key. Requests that are links, over 4 KiB, unsigned or under a name a person
  cannot have are skipped. Join prints the machine's code: the first 96 bits of the SHA-256 of
  its public key, as 24 hex digits in groups of four, so nobody can search out a key with the
  same code. The joiner tells the owner the code by some other channel. The owner runs
  `rodu team admit <name> <code>`, which refuses unless a request under that name has that code.
  That way a request someone planted in the folder under a
  teammate's name cannot be admitted by mistake. `rodu team admit` with no arguments lists
  requests waiting.
- **The root's own machine** is admitted by the root at create, so every machine of a signed
  team is listed the same way.
- **Seeing it.** On a signed team, `rodu team` syncs first, then says whether this machine is the
  root, admitted, or waiting. An unsigned team's `rodu team` is unchanged.
- **Left for later.** A join that fails after it wrote its request can leave that request in the
  folder, where it is listed until removed by hand. A request and a sync file are sealed under
  the same associated data; a request planted as a sync file opens but fails its signature, and
  is reported as damaged.
  `rodu team members` marks machines waiting for admission and lists requests. On a signed team,
  it stops calling the list unproven for admitted machines, but the name each machine gives
  stays a claim.

### Step 3: removing someone

- `rodu team remove <name>` (owner or admin, within the limits under Roles) signs a removal
  record. For each of that person's peers, it names the last file sequence this replica had
  accepted. Later files from those peers are
  refused everywhere. Operations already imported stay: the document never drops history.
- There is no global clock, so a removal can cut off edits the person made offline before it.
  Those edits are reported, not merged silently.
- On an encrypted team, removal also requires a new team key, since the removed person still holds
  the old one. Re-keying distributes the new key sealed to each remaining machine's key. That
  needs an X25519 key per machine besides the signing key, and it is the largest part of this
  step. Until it is built, `team remove` on an encrypted team says that what the person already
  had and anything written afterwards stays readable to them, and recommends a new team.

### Roles

Decided 2026-10-10: three roles, owner, admin and member.

- **Owner.** Exactly one. The creator starts as owner. The owner admits and removes anyone, grants
  and revokes admin (`rodu team admin <name> on|off`), and can hand ownership to another member
  (`rodu team transfer-owner <name>`, signed by the current owner), the same shape as the
  numbering hand-over. The new owner then has every owner right, and the old one becomes an admin.
- **Admin.** Admits new members and removes members, but cannot remove or demote another admin or
  the owner. So no two people can ever remove each other at the same time, which would otherwise
  cut both off after a merge.
- **Member.** Reads and writes the board; admits and removes nobody.

Every grant, revocation and transfer is a signed record checked back to the root key like an
admission. A revoked admin's admissions count only up to the file sequence the revocation names
for that admin's machines, the same rule as removal, so an admission made offline at the same
time as the revocation is held, not accepted. Only the current owner can transfer ownership; if
that owner signs two transfers from different machines at once, the one written by the lower peer
id wins on every replica.

## What this does not do

- **Permissions on cards.** Signing proves who wrote an operation; it does not stop a member from
  editing any card. Field-level permissions are out of scope.
- **Hiding the board from members.** Every member reads everything.
- **Undoing a member's past edits** after removal. They stay in history, attributed.

## Dependencies

`ed25519-dalek` (BSD-3-Clause) for step 2 and `x25519-dalek` (BSD-3-Clause) for step 3, both
RustCrypto/dalek crates already common in the ecosystem. Each must pass `about.toml`, `cargo
audit` and `cargo xtask notices` before it ships, including its full dependency tree.

## Open questions

1. Should a team be able to require signing from creation (`team create --signed`) and refuse
   unsigned teams entirely, rather than migrating?

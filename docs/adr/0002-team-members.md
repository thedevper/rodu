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

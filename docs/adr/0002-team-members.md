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

### Step 2: each machine signs what it writes (2a and 2b built 2026-10-10)

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

#### Step 2b as built (admins and ownership transfer; built 2026-10-10)

Where 2b differs from Roles below, 2b is what holds.

- **Rights belong to machine keys.** Owner and admin are recorded per machine key. The commands
  take a person's name and act on that person's admitted machine (found through the `members`
  entry of each admitted peer). Which person a machine belongs to is only what it says, so
  granting admin and handing the team over also take the machine's code, which the person reads
  out from their own machine, as admitting does: `rodu team admin <name> on <code>`,
  `rodu team transfer-owner <name> <code> --yes`. Revoking (`rodu team admin <name> off`) takes
  `--machine <id>` when the person has more than one.
- **Authority records.** These live in the team document's root map `authority`. Each key is the
  hex SHA-256 of its value, so a value that was changed no longer matches its key and is ignored.
  Every record is signed, and its signer is named in it:
  - `owner.<epoch>.<new owner key>.<carried records>.<signer key>.<signature>`: a transfer. It
    is valid when the owner of epoch `epoch - 1` signed it. The owner of epoch 0 is the root key.
    `carried records` lists (comma-separated, or `-`) the hashes of every admin record the owner
    signed in the epoch the transfer ends.
  - `admin.<epoch>.<n>.<target key>.on.<signer key>.<signature>`: a grant, valid when the owner
    of `epoch` signed it.
  - `admin.<epoch>.<n>.<target key>.off.<kept admissions>.<signer key>.<signature>`: a revocation,
    valid on the same rule. `kept admissions` is a comma-separated list of `<peer>:<key>`, or `-`.
  - The signatures cover `"rodu-authority-1" 0x00 || workspace id length (u64 LE) || workspace id
    || the record text up to its signer key`.
- **The owner.** Starting from the root, each epoch's owner is the new key of the valid transfer
  for that epoch. The chain stops at the first epoch with no valid transfer. Only the current
  owner can transfer (`rodu team transfer-owner <name> --yes`).
- **A second transfer for one epoch.** An owner who hands the team on still holds its key, and
  could sign another transfer for the same epoch later, to a key of its own. So each replica notes
  the transfers it follows (local note `owners`) and keeps following them: a later transfer for an
  epoch it already settled never moves it, whatever its hash. A replica that sees two at once, and
  settled neither, takes the one with the lowest record hash, as every such replica does. Either
  way it marks the team disputed, and `rodu team` warns.
- **Admins.**
  - Every key that was ever owner is an admin from the epoch it stopped being owner, as if granted
    with `n = 0`.
  - An admin record of the current epoch counts when the current owner signed it. One of an
    earlier epoch counts only when the transfer that ended that epoch carries its hash. So nothing
    a former owner signs for its own epoch after handing over counts (it cannot name admins).
  - For each target key, the counting grant or revocation with the highest `(epoch, n)` decides.
    A revocation wins a tie.
  - `rodu team admin` (current owner only) writes one record for that machine key. `n` is one more than the highest `n` for that target in the current epoch.
- **Who can admit.**
  - An admission record is now `<member key>.<signer key>.<signature>`. The 2a form, without a
    signer, means the root signed it.
  - An admission counts when its signer is the current owner, or is an admin now.
  - It also counts when the signer was an admin and the revocation that decides that signer lists
    the admission in its kept admissions.
  - When the owner revokes an admin, the revocation keeps every admission that admin signed which
    the owner's machine holds. An admission the admin signed offline at the same moment is
    therefore not kept, and the machine waits to be admitted again.
- **The root key.** In 2a a file signed by the root key was taken in from any replica folder. Now
  that holds only while the root key is the owner or an admin. Once revoked, the root's machine
  needs an admission like any other (`rodu team create --signed` admits it from the start).
- **Admitted keys are not admins.** Admins admit and (in step 3) remove members. They cannot
  grant or revoke admin, or transfer ownership. So no two people can ever shut each other out at
  once.
- **Kept once checked.** Each replica already keeps every admission it checked (2a). It now also
  keeps every authority record it checked that could ever count (see below), and it notes the
  signer of each admission it keeps, so a revocation still takes effect locally. An entry that fails its check (a key that is not its
  hash, a bad signature, a malformed text) is noted by the hash of its key and text (up to 4096),
  so it is not checked again; its text is not kept. A valid record is kept only when its signer
  is the root or a key that some kept transfer hands the team to: a record signed by any other key
  can never count, and keeping it would let anyone who can write to the folder grow the notes
  without end. (An owner, or a former owner, can still sign as many records as it likes; that is
  bounded only by the document.)
- **Left for later.** A member who deletes an authority record before some replica has read it
  can keep that replica from seeing it. For a revocation, that replica would still accept
  admissions the revoked admin signs. Only a replica that has never seen the revocation (such as
  one that joins later) is affected. Step 3 removes such a member.
- **Also left for later: a disputed hand-over.** A replica that never saw the first transfer for
  an epoch (one that joins after a former owner signed a second) may follow the second. It warns
  that the team is disputed; the owner it should follow is settled between people, and step 3
  lets the owner remove the former owner's machine.
- **Also left for later.** A pull decides whose files it takes in before it reads them. If one pull
  brings a revocation together with files from a machine the revoked admin admitted late, those
  files can land before the revocation counts. Work already taken in is never taken out again;
  later work from that machine waits. Step 3's removal covers what such a machine wrote.

### Step 3: removing someone (3a built 2026-10-10; 3b, re-keying, built 2026-10-11)

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

#### Step 3a as built (removal on a signed team; re-keying is 3b)

Where 3a differs from the text above, 3a is what holds.

- **The cut is counted in operations, not files.** File names are not signed (2a), and a
  compaction rewrites a replica's work into one new file, so a file sequence proves nothing. A
  removal names the peer and how many of its Loro operations the remover's machine holds (the
  peer's entry in its version vector). Operations of that peer past the count are refused: the
  import check, which already decodes each update in the child process, also checks that the
  update ends by the cut, and reports it as cut rather than damaged. A file going past the cut is
  not taken in and not remembered as done. It is said once ("not taken in ... written after it
  was removed") and read again only if the cut moves.
- **Removal record.** `remove.<peer, 16 hex>.<end>.<signer key>.<signature>` in the `authority`
  map, signed like the other authority records. It counts while its signer is the owner or an
  admin, and no key admitted for that peer is the owner or an admin: the owner's and admins'
  machines are never removed (the owner first revokes an admin). Where more than one counts, the
  highest end is the cut, so a remover who saw more of the peer's work keeps it. An admin can
  already admit anyone, so letting an admin raise a cut gives no new power. When the owner
  revokes an admin, it signs again each removal that admin made, so those machines stay out, as a
  revocation keeps that admin's admissions. `rodu team remove <name> [--machine <id>] --yes`
  removes the person's machines (their `members` entries), and refuses before writing anything if
  one of them is the owner's or an admin's. A removed machine cannot be admitted again, nor its
  key made an admin (an admin's machine is never cut, so that would undo the removal): it joins
  again as a new machine, from a new workspace.
- **Work a member built on is never cut.** In Loro every change depends on everything its
  machine held. If a member took in some of the removed machine's later work before it heard of
  the removal, all of that member's later work depends on it, and a replica refusing it could
  never take in anything from that member again: the team would split for good. So each machine
  that holds more of a removed peer than the removals name says so in a file of its own replica
  folder, `seen.json` (`seen.sealed`, sealed with the team key, for an encrypted team):
  `{"format": 1, "publicKey", "seen": "<peer>:<end>,...", "signature"}`, signed over
  `"rodu-seen-1" 0x00 || workspace id length (u64 LE) || workspace id || writer peer id (u64 LE)
  || text length (u64 LE) || text`. A claim counts when its signer is the owner or an admin, or is
  admitted for the replica folder it sits in and that machine is not removed. The cut moves up to
  the highest claim that counts. The claim cannot travel in the document: there it would depend on
  the very operations it lets in, and wait for them for ever. Each replica keeps the highest claim
  of each machine it counted (local note `seen`), so a claim deleted later still counts there.
- **Authority travels outside the document too.** Each authority record and admission is also
  written to the signing machine's `authority.json` (`authority.sealed`): `{"format": 1,
  "records": [...], "admissions": ["<peer>.<record>", ...]}`, read up to 1 MiB. Each entry
  verifies by its own signature, exactly as in the document, so the file adds no trust. What it
  adds is timing: a replica knows who was admitted and removed before it takes in a single
  operation. Without it, a replica catching up learns of a removal only once the operations the
  record depends on have landed, and by then it has read the removed machine's later files with
  no cut in place. It also narrows the 2b residuals: a revocation or an admission in a file is
  seen before any update is read, and deleting it from the document no longer hides it. Since a
  stranger's file can hold any number of self-signed admissions, an admission is noted only when
  its signer could ever admit (the root, a key a transfer hands the team to, or a key such a key
  granted admin). A machine writes its file afresh each time, from its notes, so a damaged file
  loses nothing.
- **A removed machine stops.** Once it hears of its removal (from a file, before it reads any
  update), its push is refused with "This machine was removed from the team", so it neither
  writes nor compacts work nobody takes in. `rodu team` says so, and `rodu team members` marks the
  machine "removed".
- **Left for later.**
  - Re-keying an encrypted team: built in 3b, below.
  - Anyone who can write to the folder can delete files there (ADR 0001), including a removed
    person whose access to the shared folder was not taken away. That includes their own files
    from before the cut, which a replica joining later then never gets, and anything built on
    them waits on such a replica. `team remove` asks for the person's folder access to be taken
    away too.
  - A removed machine that never heard of its removal and compacted puts work from before the
    cut and after it into one file, which is refused whole. A replica that already had the
    earlier files loses nothing. One that did not (one joining later, after that compaction
    removed the earlier files) misses that work.
  - A dishonest member can claim to hold more of a removed peer than it does, and so let that
    peer's later work in, as it could write that work itself. Removing that member too ends it;
    what came in before stays.
  - An owner who signs a grant of admin to a removed machine's key by hand, rather than through
    `rodu team admin`, undoes that removal: an owner's choice, not a way in for anyone else.
  - Records and admissions in a stranger's authority file are checked again on every read, as
    those in the document are. A file of 1 MiB of validly signed records costs every read their
    signature checks.

#### Step 3b as built (re-keying an encrypted signed team)

Where 3b differs from the text above, 3b is what holds.

- **An exchange key per machine, with no file of its own.** Each machine of an encrypted signed
  team has an X25519 key besides its Ed25519 signing key, as `ed25519-dalek` advises (a signing
  key is not reused for Diffie-Hellman). Its secret is the SHA-256 of `"rodu-exchange-key-1" 0x00
  || the signing key's seed`, so it needs no second secret file and every machine of a team made
  before 3b has one already. The machine publishes the public half in `exchange.sealed` in its
  replica folder: `{"format": 1, "publicKey", "exchangeKey", "signature"}`, signed over
  `"rodu-exchange-1" 0x00 || workspace id length (u64 LE) || workspace id || peer id (u64 LE) ||
  exchange key`. It writes the file with its join request and again on any pull that finds it
  missing or not its own. A key is wrapped for a machine only when the signer of its exchange file
  is a key admitted for that very replica folder, and never for a low-order point (which no wrap
  can be made for); a machine whose key cannot take a wrap is skipped, never holding up the
  others.
- **Key generations and key records.** The invite code's key is generation 0. `rodu team rekey
  --yes` (owner or admin) makes a random key of the next generation, one more than the newest this
  machine holds (two keys of one generation are settled by their checks) and names it in a signed
  authority record, `key.<generation>.<key check>.<signer key>.<signature>`, where the key check
  is the team file's `keyCheck` computed for the new key. The record goes in the `authority` map
  and the authority file like the others, counts while its signer is the owner or an admin, and is
  kept when a kept grant names its signer, as removals are. When the owner revokes an admin, it
  signs again each key record that admin made, so a machine joining later still takes those keys.
  `rodu team remove` on an encrypted team re-keys after the removals, so the new key is wrapped
  for nobody removed.
- **Wrapped keys.** An owner's or admin's machine keeps a plain `keys.json` in its replica folder:
  `{"format": 1, "keys": ["<recipient peer>.<generation>.<key check>.<wrapped, hex>", ...]}`, read
  up to 1 MiB. `wrapped` is an ephemeral X25519 public key (32 bytes), a nonce (24) and the key
  sealed with XChaCha20-Poly1305 (48). The key-encryption key is the SHA-256 of `"rodu-key-wrap-1"
  0x00 || shared secret || ephemeral public key || recipient public key`, and the associated data
  is `"rodu-key-wrap-1" 0x00 || workspace id length (u64 LE) || workspace id || recipient peer id
  (u64 LE) || generation (u64 LE)`. A shared secret of all zeros (a low-order point) is refused.
  On every pull, on `team admit` and on `team rekey`, the machine wraps every key after the first
  that it holds for every machine admitted and not removed that published an exchange key. It
  keeps the wraps already in its file only when it wrote them itself (local note `wrapped`, their
  hashes), so a wrap someone changed is made afresh. The file is plain because a machine needs no
  team key to read it. It shows the folder's provider the recipients' peer ids (already the
  replica folders' names) and how often the key changed.
- **Taking a key.** A pull first unwraps what is wrapped for this machine. It tries at most 64 new
  entries per file, so a planted file cannot make it run X25519 without end. It keeps a key only
  when the key matches the entry's check and a key record that counts names that generation and
  check, and only as a chain: a key one generation past the newest this machine holds (or of the
  same generation), taken oldest first. So no record, however high its generation, becomes the
  newest key without the ones before it: an admin cannot use up the generations, or stay the key
  everyone seals with after it is revoked and removed. Until then the key only opens the folder's
  small files (authority, seen and exchange files, whose content counts by its own signatures).
  That way a record sealed with the new key is read, and the key itself never seals anything. Kept
  keys go in `.rodu/team-keys` (one `<generation>.<check>.<key>` per line, mode 0600, zeroized in
  memory, never in `config.json`); `team.key` keeps the invite code's key. A machine seals with
  the newest key it holds (the lowest check first when two records name one generation, so every
  machine picks the same) and opens a file with whichever key it holds that opens it.
- **A file no key opens yet waits.** A sealed file that does not open with any key this machine
  holds is said once ("does not open with any team key this machine holds") and read again once
  the machine holds another key. Before 3b it was refused for good. Now a file sealed with a key
  still on its way, which a folder app may deliver first, is not lost.
- **The removed machine still hears of it.** The re-key also re-seals the remover's authority
  file with the new key, so a removed machine could no longer read its removal there. It would
  keep writing work nobody takes in. So each machine's removals also go, alone, in
  `removed.sealed` in its replica folder, sealed with the invite code's key, read like an
  authority file (only removal records count from it).
- **Seeing it.** `rodu team` says how often the team key changed and whether this machine holds
  the newest, or waits for it from the owner's or an admin's machine.
- **Left for later.**
  - A machine that has not synced since the re-key keeps sealing with the older key, which the
    removed person holds, until it next syncs with a machine that wraps the new key for it.
  - The invite code still carries the first key. What a machine writes before it is admitted
    (its request, its exchange file, its first files) is sealed with that key. A removed person
    who kept the invite code can read those, and can still read the names in later requests.
  - Anyone who can write to the folder can swap a machine's exchange file for one signed by a
    key not admitted there. It is then not used, so that machine gets no new key until its own
    next pull puts its file back: a delay, not a way in. Deleting an owner's or admin's
    `keys.json` delays the same way, until that machine's next pull writes it again.
  - What the folder held before the re-key stays readable to whoever holds the older key. A new
    key protects what comes after it, not what came before.
  - `removed.sealed` shows anyone holding the invite code's key which machines were removed, and
    by whose key.

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

`ed25519-dalek` (BSD-3-Clause) for step 2. Step 3b uses X25519 from `curve25519-dalek`
(BSD-3-Clause) directly, a crate `ed25519-dalek` already brings in, so it adds no crate to the
tree; `x25519-dalek` was not needed. Both are RustCrypto/dalek crates already common in the
ecosystem. Each must pass `about.toml`, `cargo audit` and `cargo xtask notices` before it ships,
including its full dependency tree.

## Open questions

1. Should a team be able to require signing from creation (`team create --signed`) and refuse
   unsigned teams entirely, rather than migrating?

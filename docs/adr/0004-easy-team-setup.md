# ADR 0004: Team setup in as few steps as possible

- Status: accepted 2026-10-11; steps 1 and 2 built 2026-10-11

## Context

The owner restated Rodu's stance on 2026-10-11. Rodu should be as easy to use as it can be, with
as little to set up as possible, and a team should be able to share a board without creating a
server, a VPS or anything else that costs money.

The folder transport (ADR 0001) already meets the cost side. A team syncs through a folder it
already shares, such as Google Drive, Dropbox, OneDrive, iCloud Drive or Syncthing. The relay (ADR
0003) needs a host that someone runs, so it stays an option for teams that want speed and never
becomes the default path.

The setup side has too many steps. Setting up a safe team in v0.4.0 goes like this:

| Who | Step |
|---|---|
| Owner | `rodu init --name ann --key DEMO --title "Our board"` |
| Owner | Find the shared folder's path on disk, such as `~/Library/CloudStorage/GoogleDrive-…/My Drive/board` |
| Owner | `rodu team create --folder <that path> --encrypt --signed`, and know to pick both flags |
| Owner | Send the invite code |
| Teammate | Find the same folder's path on their own machine, which is different on Windows |
| Teammate | `rodu team join - --folder <path> --name bob`, then paste the code |
| Teammate | Read the machine code the join prints and send it back to the owner |
| Owner | Check the code with the teammate, then run `rodu team admit bob <code>` |

That is eight steps, two flags someone has to know about, two folder paths to type, and a
round trip between the two people. All of it happens in a terminal, although `rodu web` already
gives people a board in the browser.

## Decision

Make the safe team the default, find the folder for the user, admit with a personal invite rather
than a code sent back, and put all of it in `rodu web`. The target is:

| Who | Step |
|---|---|
| Owner | `rodu team create`, or "Share with a team" in `rodu web`, then pick a Google Drive from a list |
| Owner | `rodu team invite bob`, or "Invite" in the web board, then send the invite |
| Teammate | `rodu team join`, or open `rodu web`, then paste the invite |

The folder transport, signing, sealing and re-keying stay as they are. Only how a team is
created, joined and admitted changes.

### 1. The safe team is the default

`rodu team create` with no flags makes an encrypted, signed team. `--no-encrypt` stays as an
opt-out. An unsigned team can no longer be created; this answers ADR 0002's open question 1. Teams
that already exist keep working unchanged.

`rodu team create` in a folder with no workspace runs `init` first. It takes the name from the
operating system's user name and derives the key from the folder's name, then says both so the user
can change them. `--name` and `--key` still override.

### 2. Rodu finds the shared folder, Google Drive first

The owner decided to start with Google Drive, both personal accounts and organisation (Google
Workspace) accounts. Other sync apps keep using `--folder` until they get the same support.

Google Drive for desktop puts each signed-in account in a known place. On macOS that is
`~/Library/CloudStorage/GoogleDrive-<account>/`. On Windows it is a drive letter, `G:` by default,
with one letter per account. Each account root holds:

- `My Drive`, in every account;
- `Shared drives/<drive>`, in organisation accounts that use shared drives.

The exact locations, including mirror mode and changed drive letters, are checked against Drive
for desktop in step 1 and pinned in tests.

**Creating.** When `--folder` is not given, `rodu team create` lists every account it found, as
"My Drive (ann@gmail.com)" and "Shared drive Engineering (ann@company.com)", and asks which to
use. The team's files go in `Rodu/<collection key>/` inside the choice. For an organisation,
rodu suggests a shared drive first, because every member of the drive sees it at the same place
and the organisation owns the files. With a personal account, the owner shares the `Rodu/<key>`
folder with the team in Google Drive, as they would share any folder.

**Joining.** A teammate's copy of the folder is not always at the same path. A folder shared from
someone's personal My Drive only appears in a teammate's Drive once they add a shortcut to it, and
they choose where. So `join` does not rely on a path. It searches each Google Drive root it finds
(`My Drive` and every `Shared drives/<drive>`) a few levels deep for a `rodu-team.json` whose
workspace id matches the invite. It lists folders and reads only files with that name, and stops
after a bounded number of folders. If nothing matches, it tells the teammate what to do next:
open the shared folder in Google Drive and add a shortcut to My Drive, or ask to be added to the
shared drive, wait for Drive to sync it, then run `join` again. `--folder` still overrides.

Rodu never calls a Google API and never signs in to Google. Drive for desktop syncs the files, as
any folder app does. That keeps setup at installing Drive for desktop, which an organisation on
Google Workspace usually already has.

**Permissions on a shared drive.** Rodu replaces its own files (a temporary file renamed over the
old one) and deletes its own old files when it compacts. Step 1 checks which shared-drive role
allows both, Contributor or Content manager. It then says which role members need, and gives a
clear message when a write is refused.

### 3. A personal invite admits on its own

Today, anyone holding the invite code and access to the folder can ask to join, so the owner
checks each machine's code with the person before admitting it. That check is the round trip.

`rodu team invite <name>` makes an invite for one person. On top of what an invite already holds
(workspace, owner key, team key), it carries a random 128-bit **join secret** that
only the owner's machine knows. The owner's machine keeps it as a pending invite, along with the
name and an expiry of 7 days by default.

When the teammate joins with it, their request carries a proof: a MAC made with the join secret
over the workspace, the peer id, the machine's public key and the name. On a later pull, the
owner's or an admin's machine admits the request automatically when all of these hold:

- the proof checks against a pending invite;
- the invite has not expired;
- the invite has not been used.

The invite is then used up, so it admits exactly one machine. A request without a valid proof
still waits for `rodu team admit`, as it does today.

The join secret is as sensitive as the invite. A single-use invite with an expiry limits the damage
if one leaks: it admits one machine, which `rodu team members` shows and `rodu team remove` takes
out. `rodu team invites` lists pending invites, and `rodu team invite --revoke <name>` cancels one.

The owner's machine must pull for the admission to happen. `rodu web` and `rodu mcp` already pull
every few seconds, and any command pulls, so in practice the teammate is in as soon as the owner
uses Rodu.

### 4. All of it in `rodu web`

The web board gets a team screen. It offers to create a team, with the list of Google Drives from
step 2. It shows the members and the invites, with a button to invite someone and copy the invite.
It lets a user join by pasting an invite. It can remove someone, asking for confirmation. Every
action calls the same code as the CLI, through the local API that already requires the per-run
token.

### Steps

Each step ships alone.

1. **Defaults and Google Drive.** Encrypted and signed by default, `create` that initialises an
   empty workspace, finding Google Drive accounts and shared drives, and `join` finding the team's
   folder by its workspace id. Tests use stand-in folders laid out like Drive for desktop's, and no
   real account. The owner checks the result once by hand on a real personal account and a real
   Workspace account.
2. **Personal invites.** `team invite`, `team invites`, `--revoke`, the proof in a join request,
   and automatic admission on owner and admin machines. Tests cover a wrong, expired, reused and
   revoked invite, and a request carrying a proof from another workspace.
3. **The team screen in `rodu web`.**

### Step 1 as built

- **Defaults.** `rodu team create` with no flags makes an encrypted, signed team. Unlike the
  decision above, `--unsigned` stays as an explicit opt-out, next to `--no-encrypt`: unsigned teams
  made before this step are still supported, and the opt-out keeps their behaviour reachable and
  tested. `--signed` is still accepted. `--signed` and `--unsigned` together are refused.
- **No workspace yet.** `create` runs `init` first. The name comes from `--name`, else `USER` or
  `USERNAME` (letters, digits, `.`, `_` and `-` kept). The key comes from `--key`, else from the
  folder's name (its letters and digits in capitals, starting with a letter, at most 10, else
  `TEAM`). The title comes from `--title`, else the folder's name. It says which name and key it
  used. A system user name is cut to 34 characters, so that `<name>-agent` fits. The name, key and
  title are checked, and the chosen Drive folder is checked for another team, before anything is
  written, so a create that cannot finish leaves no workspace behind.
- **Inside a workspace.** Run from a folder below a workspace, `create` would share all of that
  workspace, so it refuses unless given `--yes` (or `RODU_DIR`).
- **Finding Google Drive** (`crates/rodu/src/drive.rs`) looks at names on disk only:
  - macOS: `~/Library/CloudStorage/GoogleDrive-<account>/`;
  - Windows: drive letters D to Z whose root holds `My Drive`, or holds one folder per account
    that does;
  - a mirrored `My Drive` in the home folder.

  Under an account it offers `My Drive` and each `Shared drives/<drive>`, with shared drives
  first. An account whose folders have other names, as in another language, has each of its
  folders offered as it is, except `Other computers`.
- **Choosing.** With one location, `create` uses it and says so. With several, it lists them and
  asks for `--drive <number>`, rather than prompting, so that the same command works in scripts and
  tests. A prompt can come with the web screen (step 3). Files go in `Rodu/<collection key>`.
- **Joining.** With no code and a terminal, `join` asks for the code, as `-` does. With no
  `--name`, it uses the system user name. With no `--folder`, it searches every location
  breadth-first, at most 4 levels deep and in at most 5,000 folders. It skips hidden folders and
  `Other computers`, and follows shortcuts. It looks in each location's `Rodu/` folder first, where
  `create` puts teams, and stops there when the team is found. It looks for a `rodu-team.json`
  holding the invite's workspace id, and never looks inside a team's folder, unless that file does
  not read. A match counts only once its real path, with links followed, is inside a location, and
  a folder reached two ways counts once. None found means a message saying how to get the folder
  into Drive. Several found means a list and a request for `--folder`. A search cut short by the
  folder limit never takes a single match on its own, because another could be past the limit: it
  asks for `--folder`. The join prints the folder it used, so a teammate can see it is the right one.
  A name already on the team gets a hint to use `--as` from a second machine.
- **Windows in another language.** An account root is recognised by `My Drive` or by Drive's own
  `.shortcut-targets-by-id` folder, which keeps its name in every language.
- **Tests.** The tests use stand-in homes laid out like Drive for desktop's, with no real account.
  `drive.rs` has unit tests for macOS accounts, drive letters, mirror mode, folder names in another
  language and the bounded search. `tests/team.rs` covers:
  - create and join with no folder typed, through a shortcut, then admit, then a card from the
    teammate arriving;
  - the list and `--drive`;
  - no Drive at all, the team's folder not synced yet, and no code given.
- **Not yet checked on a real account.** The Drive for desktop layout comes from Google's help
  and common knowledge, not a real install on this machine: mirror mode's location, how Windows
  shows several accounts, and whether `My Drive` and `Shared drives` keep their English names in
  other languages. Shared-drive roles also come from Google's help pages: Contributors cannot
  delete or move files, and Content managers can. The owner checks once by hand on a personal
  account and on a Workspace account with a shared drive, and any difference becomes a fix.
- **Known limits.**
  - Looking at drive letters D to Z on Windows can stall for a moment on a disconnected network
    drive.
  - On a Windows developer machine, the tests that run `rodu` look at the real drive letters,
    because those do not come from `HOME`. They find no team there and write nothing, but they
    can see a real Drive.
  - Anyone who can write into a teammate's Drive could plant a `rodu-team.json` with the same
    workspace id. The join then finds two and asks for `--folder`; an encrypted or signed team's
    files would not open or check in the planted folder anyway.

### Step 2 as built

- **The invite.** `rodu team invite <name> [--days N]` runs on the owner's or an admin's machine
  of a signed team. It prints the team's invite code, with the team key on an encrypted team, then
  `+<join secret>.<name>`: 32 hex digits of a random 128-bit secret, then the name. It lasts 7 days
  by default, and from 1 to 90. Inviting the same name again replaces the earlier invite, so only
  the newest works. At most 256 invites wait at once.
- **Where it is kept.** The machine that made the invite keeps it in `.rodu/invites`, a private
  file (mode 0600 on Unix) with one `<expires>.<secret>.<name>` line per invite. The secret is in
  no team file. Only that machine admits with it: an admin's invite is not known to the owner's
  machine.
- **The proof.** `join` with a personal invite asks under the invite's name, and refuses another
  `--name` or `--as` before writing anything. Its request carries a proof, HMAC-SHA256 under the
  join secret over a domain string (`rodu-join-1`), the workspace id, the peer id, the machine's
  public key and the name, each with its length where it varies. The request is signed and, on an
  encrypted team, sealed, as before. A request with no proof is read as before, and older versions
  ignore the proof.
- **Admitting.** Each pull on the machine holding invites, once its keys are settled and before it
  reads the other machines' files, looks at every request with a proof from a machine not yet
  admitted or removed. It admits one when the name matches a held invite, the invite has not
  expired, and the proof checks in constant time. The invite is removed from the file before the
  admission is written, so a crash in between uses the invite up rather than leaving it to admit a
  second machine. The admitted machine's files land in that same pull. The CLI says so as `note:
  sync: admitted bob's machine …`; `rodu web` and `rodu mcp` write it with their other sync lines.
  A machine that may no longer admit (an admin who was revoked) admits no one through its invites.
- **Listing and cancelling.** `rodu team invites` lists the invites not used yet, with the UTC day
  each ends, and shows expired ones as expired. `rodu team invite --revoke <name>` cancels one.
  Expired invites stay listed until cancelled or replaced.
- **Fallback.** A request whose proof does not check (a wrong secret, another name, an expired or
  cancelled invite, a proof made for another workspace) waits for `rodu team admit`, as before.
- **Tests.** `rodu-sync` covers admission in the owner's pull with the files landing at once, an
  admin's own invite, one admission per invite (also with two machines in the same pull), and
  refusal for a wrong secret, another name, a revoked, expired or replaced invite, and a proof for
  another workspace. It also checks that only the owner or an admin invites, the name and days
  checks, and the private mode of the invites file. `tests/team.rs` covers the CLI on an encrypted
  team end to end, the name check at join, revoking, and who may invite. Each check was confirmed
  by removing it and watching a test fail.
- **Known limits.**
  - The invite holds the join secret next to the team key, so anyone who sees it before the
    teammate uses it can join in their place. Single use, the expiry and `rodu team members` bound
    this, as the decision says.
  - Admission happens when the inviting machine pulls. If that machine is off, the teammate waits.
    The request also waits in the folder, so the owner or an admin can admit it by hand with
    `rodu team admit`.

## What this does not do

- **Faster sync.** Changes still arrive as fast as the folder app moves them. Sync between machines
  on the same LAN, which needs no server, is a separate decision.
- **Card numbers.** Cards made on a teammate's machine still wait for the numbering machine.
- **Replace the relay.** ADR 0003 stays as an option for a team that wants to run one.

## Dependencies

None. Finding Google Drive's folders uses the standard library. The join secret's MAC is HMAC
(RFC 2104) built on `sha2`, which Rodu already ships, and checked against RFC 4231's test vectors,
so no `hmac` crate was added. Anything new must pass `about.toml`, `cargo audit` and `cargo xtask
notices`, across its whole tree.

## Open questions

1. Should the invite be a link, such as `rodu://join?…`, that opens Rodu when clicked? That needs
   a URL handler registered on each OS, which adds setup to installing.
2. Should a personal invite's default expiry be shorter than 7 days?
3. Should `team create` on a machine with no sync app suggest one, or just say what it looked for?

# ADR 0003: A relay for live sync

- Status: proposed 2026-10-11

## Context

A team syncs through a folder it shares (ADR 0001): each machine writes its own files under
`sync/<peer>/` and reads everyone else's. How fast a change arrives depends on the folder's
provider, which takes from a few seconds to minutes. `rodu web` and `rodu mcp` already pull every
two or three seconds, so the provider is the slow part.

ADR 0001, step 5, names `rodu relay` for teams that want changes to arrive in under a second,
carrying the same files as the folder. ADR 0002 comes first so that a relay can refuse files from
anyone who is not a member. With ADR 0002 built:

- every machine has an Ed25519 machine key, and every file it writes is signed with it;
- the authority records (admissions, revocations, admin grants, team-key records) are signed
  statements that anyone holding the owner's public key can check, without the team key;
- an encrypted team seals every file with the team key, which changes after a removal (step 3b).

The owner decided three things for this design (2026-10-11):

1. **Every request is signed** with the machine key. The relay holds the list of admitted machine
   keys and never holds the team key, so it cannot read an encrypted board.
2. **The relay does TLS itself**, with rustls, rather than relying on a reverse proxy.
3. **The relay adds to the folder and does not replace it.** The folder stays where the team's
   files live. If the relay is down, sync still works through the folder, only slower.

## Decision

`rodu relay serve` runs a relay that a team hosts itself. It stores copies of the same files the
folder holds, accepts each one only from the machine whose folder it belongs to, and tells
listening machines the moment something new arrives. It builds on what is already in the tree:
axum 0.8, hyper 1, tokio, and the signing and authority code in `rodu-sync`.

### What the relay holds

For each team it serves:

- **The owner's public key and the workspace id.** These are set when the operator allows the
  team, and the relay never accepts a team it was not told about.
- **The authority records**, uploaded by any admitted machine. They are the same signed records
  as `authority.json`, sent in the clear even on an encrypted team. The relay keeps the union of
  every valid record it has been given, so a removed machine cannot hide its own revocation by
  uploading an older set. It reaches the admitted machines with the same code as `Authority` in
  `rodu-sync`. The records are capped at 1 MiB, the same as the authority file.
- **Copies of the files under `sync/<peer>/`**, stored on disk with the folder's layout, so an
  operator can inspect them or copy them back into a folder. Each file is still signed, and sealed
  on an encrypted team. The relay checks the request signature but not the file's contents.

The relay does not hold `rodu-team.json`, join requests, or any key that opens a sealed file.

### Requests

The base URL is `https://<host>:<port>/v1/<workspace id>/`.

| Method and path | What it does | Who may call |
|---|---|---|
| `PUT authority` | Merge signed authority records into the relay's union | any admitted machine |
| `PUT sync/<peer>/<name>` | Store one file in that peer's folder | the machine admitted as `<peer>` |
| `DELETE sync/<peer>/<name>` | Remove one, as compaction does in the folder | the machine admitted as `<peer>` |
| `GET sync/<peer>/<name>` | Fetch one file | any admitted machine |
| `GET changes?after=<n>` | List files changed after cursor `n`, waiting up to 25 s if none have | any admitted machine |

`<name>` must match the names the folder transport writes (`*.update`, `seen.*`, `authority.*`,
`removed.sealed`, `exchange.sealed`, `keys.json`; the last three come with step 3b, PR #15), so
the relay never stores an arbitrary path. Each name has the same size cap as in the folder, and an
update the same cap as an import. Updates are capped by a per-peer quota that the
operator sets.

`changes` returns `[{peer, name, seq}]` with a cursor that only goes up. A waiting `changes` call
returns as soon as a matching `PUT` or `DELETE` completes. That is the sub-second path: no
watching of the folder's provider is involved.

### Signed requests

Every request carries a header:

```
Rodu-Signature: <peer>.<unix seconds>.<nonce hex>.<signature hex>
```

The signature is the machine key's signature over a message with its own domain, so it can never
be confused with a file or an authority record:

```
"rodu-relay-1\0" || len(workspace) || workspace || peer (u64 BE) || unix seconds (u64 BE)
  || nonce (16 bytes) || method || "\0" || path and query || "\0" || SHA-256(body)
```

The relay accepts a request only when all of these hold:

1. The workspace is one the operator allowed.
2. `<peer>` is admitted in the relay's authority union, and the signature checks against the
   machine key admitted for that peer. A revoked or removed machine is refused from the moment
   its revocation reaches the relay.
3. The time is within 120 seconds of the relay's clock, and the nonce has not been seen in that
   window. The relay keeps nonces for 240 seconds, so a captured request cannot be replayed.
4. For `PUT` and `DELETE` on `sync/<peer>/…`, `<peer>` is the signer's own peer. This is the same
   rule as the folder check, which refuses a file holding operations of any peer but the one its
   folder names.

A refused request gets `401` or `403` with no detail beyond the status, and is not logged with
its body.

### TLS

The relay terminates TLS itself with rustls, so a team can put it on the internet without anything
in front of it.

- **Certificate.** By default, `rodu relay serve` makes a self-signed certificate on first start,
  keeps it in its data folder, and prints its SHA-256 fingerprint. An operator with a domain can
  instead pass `--cert` and `--key` PEM files, for example from Let's Encrypt.
- **Pinning.** A machine adds the relay with
  `rodu team relay add https://host:7443 --pin sha256:<fingerprint>`. Rodu's client checks that the
  server's certificate matches the pin, and does not use the system's or a bundled CA list. This
  keeps the client the same whether or not the operator has a domain, and needs no CA roots in the
  tree.
- **Plain HTTP** is accepted only on a loopback address, for tests.

TLS hides the request headers and the list of files from the network. The files were already
signed and, on an encrypted team, sealed, so TLS is not what keeps the board private.

### On the machines

- **Writing.** `push` writes to the folder as today. When a relay is set, it then sends the same
  file with `PUT`, with a short timeout. A relay that cannot be reached is noted and does not fail
  the command, since the folder already has the file.
- **Reading.** Files fetched from the relay go into a cache, `.rodu/relay/<workspace>/`, laid out
  like the team folder. `pull` reads the folder and the cache as one, so every file goes through
  the same checks (signature, peer, seal, authority) whichever way it came. A file with the same
  name in both places must have the same bytes, or it is refused as a conflict. Rodu never writes
  relay files into the shared folder, because the provider would sync them a second time and could
  make conflicting copies.
- **Live.** `rodu web` and `rodu mcp` hold one `changes` call open and pull as soon as it returns,
  in place of waiting for the next tick. With no relay, or with the relay down, they keep their
  current two to three second timer.
- **Authority.** After each pull, a machine sends its authority records with `PUT authority` if
  they changed since it last sent them. An owner or admin machine does this right after `admit`,
  `remove` and `set_admin`, so a removal reaches the relay in under a second.

### Operating it

```
rodu relay serve --data <dir> [--listen 0.0.0.0:7443] [--cert <pem> --key <pem>]
rodu relay allow <invite code or owner key>    # add a team this relay will serve
rodu relay deny <workspace id>                  # stop serving it; its files are deleted
```

The relay runs as its own process, so the CLI commands stay short. It keeps everything under
`--data` and needs nothing else on the host. Per-team limits on storage, open `changes` calls and
requests per minute come from flags with defaults suited to a small team.

### Steps

Each step ships alone.

1. **The server.** `rodu relay serve`, `allow` and `deny`; TLS with a pinned self-signed
   certificate or a given one; signed requests; the authority union; `PUT`, `GET`, `DELETE` and
   `changes`. Tests run the relay on loopback and check that it refuses every request it should,
   including replays, a removed machine, a machine writing another peer's folder, and names
   outside the allowed set.
2. **The client.** `rodu team relay add/remove/status`, pinned TLS, the `PUT` after `push`, the
   cache, and `pull` reading folder and cache together.
3. **Live.** `changes` in `rodu web` and `rodu mcp`.

## What this does not do

- **Replace the folder.** A team needs the folder to join (requests and `rodu-team.json` are not
  on the relay) and as the place its files live. Joining through the relay can come later.
- **Hide who is on the team from the relay's operator.** The relay sees the authority records,
  which name machine keys and peer ids, and it sees which machine writes when and how much. It
  never sees what an encrypted team writes. A team that runs its own relay is its own operator.
- **Keep a history.** The relay holds what the folder holds. Backing up the team is backing up the
  folder.
- **Stop a member from flooding it.** Quotas and rate limits bound the damage. An admitted machine
  that abuses the relay is dealt with by removing it, as in the folder.

## Dependencies

TLS needs crates the tree does not have yet. Exact versions are fixed in step 1, after
`cargo audit`, `cargo xtask notices` and the full dependency tree are checked:

| Crate | Licence | Why |
|---|---|---|
| `rustls` | Apache-2.0 OR ISC OR MIT | TLS for the relay and its client |
| `tokio-rustls` | MIT OR Apache-2.0 | rustls over tokio's sockets |
| `rustls-pki-types` | MIT OR Apache-2.0 | certificate and key types |
| `ring` | Apache-2.0 AND ISC | rustls's crypto provider |
| `rustls-webpki` | ISC | certificate parsing and checks in rustls |
| `untrusted` | ISC | input parsing, used by `ring` and `rustls-webpki` |
| `rcgen` | MIT OR Apache-2.0 | the self-signed certificate |

**ISC is not in `about.toml` yet, so accepting it is a decision this ADR asks for.** ISC is a
short permissive licence, close to MIT, and its notice goes into `THIRD-PARTY-NOTICES.txt` like
any other. `ring` is chosen over rustls's default provider, `aws-lc-rs`, because `aws-lc-sys` also
carries the OpenSSL licence and needs CMake to build on Windows. `ring` builds with the C compiler
CI already has.

The relay's HTTP side uses axum, hyper and tokio, which Rodu already ships.

## Open questions

1. Should the relay be a separate binary, `rodu-relay`, so the TLS server crates stay out of the
   binary every user downloads? This is measured in step 1, with the size it adds to `rodu`.
2. Should a machine also accept a relay certificate signed by a public CA without a pin, using the
   platform's verifier? That brings in more crates and possibly the CDLA-Permissive-2.0 licence
   of `webpki-roots`.
3. Should joining work through the relay, for a team with no shared folder at all? That would
   make the relay a place the team's files live, which the owner decided against for now.

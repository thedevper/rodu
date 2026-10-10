//! Team workspaces (ADR 0001): `rodu team create`, `rodu team join`, `rodu sync`, and the sync
//! every command does around its work in a team workspace.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rodu_core::ids::{is_uuid, uuidv7};
use rodu_core::store::{Store, TxMode};
use rodu_core::{Actor, LiveSync, PrincipalKind, Pulled, Result, RoduError, RoduService};
use rodu_sync::folder::{Admitted, JoinRequest, PullReport, SYNC_DIR, TEAM_FILE, TeamFolder};
use rodu_sync::seal::TeamKey;
use rodu_sync::sign::{MachineKey, PublicKey};
use rodu_sync::{Checker, LoroStore};
use serde::{Deserialize, Serialize};

use crate::readable;
use crate::store::AnyStore;
use crate::{
    Args, Config, Io, Workspace, create_private_dir, find_dir, open, resolve, var,
    write_private_file,
};

/// The hidden command the import check runs in: `rodu __check-import` reads a framed import on
/// stdin, replays it, and exits 0 if it is valid, 2 if not (anything else: it crashed).
pub const CHECK_COMMAND: &str = "__check-import";
const INVITE_PREFIX: &str = "rodu1-";
/// An encrypted team's key, in the workspace folder; never in `config.json`.
const KEY_FILE: &str = "team.key";
/// A signed team: this machine's private signing key (ADR 0002, step 2a).
const IDENTITY_FILE: &str = "identity.key";
const SIGNED_INVITE_PREFIX: &str = "rodu2-";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamConfig {
    /// The team's sync folder, absolute.
    pub folder: String,
    pub workspace_id: String,
    /// Whether this machine is the team's numbering peer.
    pub numbering: bool,
    /// Whether the team's files are sealed with the key in `team.key`. This setting, not the
    /// folder, decides how the workspace syncs.
    #[serde(default)]
    pub encrypted: bool,
    /// A signed team: its root public key, from the invite code (or this machine's own key on
    /// the machine that created it). This setting, not the folder, decides that files are signed
    /// and which root is trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<String>,
}

/// The team key in `dir`, if there is one.
fn load_key(dir: &Path) -> Result<Option<TeamKey>> {
    let path = dir.join(KEY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => zeroize::Zeroizing::new(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RoduError::invalid(format!("Cannot read {}: {e}", path.display()))),
    };
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| TeamKey::from_hex(text.trim()))
        .map(Some)
        .ok_or_else(|| RoduError::invalid(format!("{} does not hold a team key", path.display())))
}

/// This machine's signing key in `dir`, if there is one.
fn load_identity(dir: &Path) -> Result<Option<MachineKey>> {
    let path = dir.join(IDENTITY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => zeroize::Zeroizing::new(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RoduError::invalid(format!("Cannot read {}: {e}", path.display()))),
    };
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| MachineKey::from_hex(text.trim()))
        .map(Some)
        .ok_or_else(|| {
            RoduError::invalid(format!("{} does not hold a machine key", path.display()))
        })
}

fn save_identity(dir: &Path, key: &MachineKey) -> Result<()> {
    let path = dir.join(IDENTITY_FILE);
    write_private_file(&path, &secret_line(&[key.to_hex().as_str(), "\n"]))
        .map_err(|e| RoduError::invalid(format!("Cannot write {}: {e}", path.display())))
}

fn save_key(dir: &Path, key: &TeamKey) -> Result<()> {
    let path = dir.join(KEY_FILE);
    write_private_file(&path, &secret_line(&[key.to_hex().as_str(), "\n"]))
        .map_err(|e| RoduError::invalid(format!("Cannot write {}: {e}", path.display())))
}

/// `parts` joined in a buffer that is wiped when dropped, sized up front so no copy is left
/// behind by a reallocation.
fn secret_line(parts: &[&str]) -> zeroize::Zeroizing<String> {
    let mut line =
        zeroize::Zeroizing::new(String::with_capacity(parts.iter().map(|p| p.len()).sum()));
    for part in parts {
        line.push_str(part);
    }
    line
}

/// The team folder as this workspace syncs it: sealed with its key when it is encrypted. A key
/// that is missing, or there when the workspace is plain, stops the sync rather than guess.
fn team_folder(dir: &Path, team: &TeamConfig) -> Result<TeamFolder> {
    // The fix is in the message itself: the warnings before and after a command show no hint.
    const REJOIN: &str = "join the team again with the invite code, in a new folder";
    let key = load_key(dir).map_err(|e| RoduError::invalid(format!("{}; {REJOIN}", e.message)))?;
    let folder = match (team.encrypted, key) {
        (true, Some(key)) => TeamFolder::sealed(&team.folder, key).expecting(&team.workspace_id),
        (false, None) => TeamFolder::new(&team.folder).expecting(&team.workspace_id),
        (true, None) => {
            return Err(RoduError::invalid(format!(
                "this workspace's team key is missing ({}); {REJOIN}",
                dir.join(KEY_FILE).display()
            )));
        }
        (false, Some(_)) => {
            return Err(RoduError::invalid(format!(
                "{} is here, but this workspace syncs in plain",
                dir.join(KEY_FILE).display()
            )));
        }
    };
    let Some(root) = &team.signing else { return Ok(folder) };
    let root = PublicKey::from_hex(root).ok_or_else(|| {
        RoduError::invalid(format!("this workspace's root key is not a key; {REJOIN}"))
    })?;
    let identity = load_identity(dir)
        .map_err(|e| RoduError::invalid(format!("{}; {REJOIN}", e.message)))?
        .ok_or_else(|| {
            RoduError::invalid(format!(
                "this workspace's machine key is missing ({}); {REJOIN}",
                dir.join(IDENTITY_FILE).display()
            ))
        })?;
    Ok(folder.signed(identity, root))
}

/// `rodu1-<workspace id>`, then `.<key>` for an encrypted team; a signed team's is
/// `rodu2-<workspace id>.<root public key>`, then `.<key>` for an encrypted one.
fn invite_code(
    workspace_id: &str,
    root: Option<&str>,
    key: Option<&TeamKey>,
) -> zeroize::Zeroizing<String> {
    let mut code = match root {
        Some(root) => secret_line(&[SIGNED_INVITE_PREFIX, workspace_id, ".", root]),
        None => secret_line(&[INVITE_PREFIX, workspace_id]),
    };
    if let Some(key) = key {
        code = secret_line(&[code.as_str(), ".", key.to_hex().as_str()]);
    }
    code
}

/// What an invite code holds.
struct Invite {
    workspace_id: String,
    /// A signed team's root key.
    root: Option<PublicKey>,
    /// An encrypted team's key.
    key: Option<TeamKey>,
}

fn parse_invite(code: &str) -> Option<Invite> {
    let code = code.trim();
    let (root, rest) = match code.strip_prefix(SIGNED_INVITE_PREFIX) {
        Some(rest) => {
            let (id, after) = rest.split_once('.')?;
            let (root, key) = match after.split_once('.') {
                Some((root, key)) => (root, Some(key)),
                None => (after, None),
            };
            (Some(PublicKey::from_hex(root)?), (id, key))
        }
        None => {
            let rest = code.strip_prefix(INVITE_PREFIX)?;
            (None, rest.split_once('.').map_or((rest, None), |(id, key)| (id, Some(key))))
        }
    };
    let (id, key) = rest;
    let key = match key {
        Some(key) => Some(TeamKey::from_hex(key)?),
        None => None,
    };
    is_uuid(id).then(|| Invite { workspace_id: id.to_owned(), root, key })
}

/// Runs the import check in a child process of this very executable.
pub fn checker() -> Checker {
    Checker::new(
        || {
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rodu"));
            let mut command = Command::new(exe);
            command.arg(CHECK_COMMAND);
            command
        },
        Duration::from_secs(60),
    )
}

fn warn(io: &mut Io<'_>, message: &str) {
    (io.err)(&format!("warning: sync: {message}"));
}

fn user_actor(config: &Config) -> Actor {
    Actor { principal_id: config.user_id.clone(), via_agent_id: None }
}

/// Before a command in a team workspace: takes in what teammates wrote, then, on the numbering
/// peer, numbers new cards. Problems are warnings: the command runs on what this machine has.
pub(crate) fn before(io: &mut Io<'_>, ws: &Workspace) {
    if ws.service.store.team().is_none() {
        return;
    }
    let pulled = pull_folder(io, ws);
    match settle_numbering(&ws.dir, &ws.config, &ws.service, pulled) {
        Ok(moved) => moved.iter().for_each(|line| warn(io, line)),
        Err(e) => warn(io, &format!("numbering new cards: {}", e.message)),
    }
    if let Err(e) = settle_member(&ws.config, &ws.service) {
        warn(io, &format!("recording this machine as a member: {}", e.message));
    }
    if ws.service.numbering()
        && let Err(e) = ws.service.assign_numbers(&user_actor(&ws.config))
    {
        warn(io, &format!("numbering new cards: {}", e.message));
    }
    if let Some(team) = &ws.config.team {
        refresh_copy(&ws.dir, team, &ws.service).iter().for_each(|line| warn(io, line));
    }
}

/// Brings the team's readable copy up to date, or removes it once it is turned off. Only the
/// numbering machine writes it, and only into a team folder that is there.
fn refresh_copy(dir: &Path, team: &TeamConfig, service: &RoduService<AnyStore>) -> Vec<String> {
    let Some(store) = service.store.team() else { return Vec::new() };
    let root = Path::new(&team.folder);
    if !service.numbering() || !root.join(SYNC_DIR).is_dir() {
        return Vec::new();
    }
    match store.readable_copy() {
        Ok(on) => readable::refresh(root, dir, service, on, &store.version()),
        Err(e) => vec![format!("readable copy: {}", e.message)],
    }
}

/// The warning shown whenever the readable copy is turned on.
const READABLE_WARNING: &str = "The readable copy is plain Markdown in the team folder's readable/ \
     folder and is never encrypted: anyone with access to the folder can read the board";

/// Takes in what teammates wrote; false if the folder could not be read (a warning).
fn pull_folder(io: &mut Io<'_>, ws: &Workspace) -> bool {
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return false;
    };
    match team_folder(&ws.dir, team).and_then(|folder| folder.pull(store, &checker())) {
        Ok(report) => {
            warn_report(io, &report);
            true
        }
        Err(e) => {
            warn(io, &format!("{} (working offline)", e.message));
            false
        }
    }
}

/// Settles which machine numbers cards, after a pull: the one the team document names. A team
/// made before the document named one keeps the config's choice, and that machine names itself,
/// but only after a pull that worked: a hand-over on its way must arrive first, or naming itself
/// could undo it. Turns the service's numbering on or off to match and records it in the config,
/// so the next command starts right. Returns a warning when the role moved away from this machine.
fn settle_numbering(
    dir: &Path,
    config: &Config,
    service: &RoduService<AnyStore>,
    pulled: bool,
) -> Result<Option<String>> {
    let (Some(team), Some(store)) = (&config.team, service.store.team()) else { return Ok(None) };
    let named = store.numbering_peer()?;
    let here = match named {
        Some(peer) => peer == store.peer(),
        None if team.numbering => {
            if pulled {
                store.set_numbering_peer(store.peer())?;
            }
            true
        }
        None => false,
    };
    let was = service.numbering();
    service.set_numbering(here);
    // The service starts from the config, so a change of role since then is one to record.
    if here != was {
        let mut config = config.clone();
        if let Some(team) = &mut config.team {
            team.numbering = here;
        }
        save_config(dir, &config)?;
    }
    Ok(match named {
        Some(peer) if was && !here => Some(format!(
            "card numbering moved to machine {peer:016x}; new cards here get provisional keys \
             until it numbers them"
        )),
        _ => None,
    })
}

/// Records in the team document that this machine writes for the config's person, when it does
/// not say so already: a team made before the document listed members, or an entry another
/// machine overwrote. A person the document does not hold yet is left for a later sync.
fn settle_member(config: &Config, service: &RoduService<AnyStore>) -> Result<()> {
    let Some(store) = service.store.team() else { return Ok(()) };
    let mine = store.members()?.get(&store.peer()) == Some(&config.user_id);
    if !mine && service.store.list_principals()?.iter().any(|p| p.id == config.user_id) {
        store.set_member(store.peer(), &config.user_id)?;
    }
    Ok(())
}

/// After a command in a team workspace: writes this machine's new changes to the folder.
pub(crate) fn after(io: &mut Io<'_>, ws: &Workspace) {
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else { return };
    match team_folder(&ws.dir, team).and_then(|folder| folder.push(store)) {
        Ok(pushed) => pushed.warnings.iter().for_each(|line| warn(io, line)),
        Err(e) => warn(io, &format!("{} (your changes stay here and go out next time)", e.message)),
    }
    refresh_copy(&ws.dir, team, &ws.service).iter().for_each(|line| warn(io, line));
}

/// After `web` or `mcp` stop: their store is gone, so the workspace is opened again to push.
pub(crate) fn after_reopen(io: &mut Io<'_>, via_agent: bool) {
    match open(io, via_agent) {
        Ok(ws) => after(io, &ws),
        Err(e) => warn(io, &e.message),
    }
}

fn warn_report(io: &mut Io<'_>, report: &PullReport) {
    for line in report_lines(report) {
        warn(io, &line);
    }
}

/// What a pull found that the user should hear about.
fn report_lines(report: &PullReport) -> Vec<String> {
    let skipped =
        report.damaged.iter().chain(&report.batch.refused).map(|l| format!("skipped {l}"));
    let index = report.batch.index.problems.iter().chain(&report.batch.index.conflicts);
    skipped.chain(report.missing.iter().chain(&report.batch.notes).chain(index).cloned()).collect()
}

/// Live sync with the team folder while `rodu web` or `rodu mcp` runs: the same pull, numbering
/// and push every other command does before and after it runs.
pub(crate) struct FolderLive {
    dir: PathBuf,
    config: Config,
    team: TeamConfig,
    user: Actor,
}

/// The live sync of a team workspace; `None` for a plain one.
pub(crate) fn live(ws: &Workspace) -> Option<std::sync::Arc<dyn LiveSync<AnyStore>>> {
    let team = ws.config.team.clone()?;
    ws.service.store.team()?;
    Some(std::sync::Arc::new(FolderLive {
        dir: ws.dir.clone(),
        config: ws.config.clone(),
        team,
        user: user_actor(&ws.config),
    }))
}

impl LiveSync<AnyStore> for FolderLive {
    fn pull(&self, service: &RoduService<AnyStore>) -> Result<Pulled> {
        let Some(store) = service.store.team() else { return Ok(Pulled::default()) };
        let report = team_folder(&self.dir, &self.team)?.pull(store, &checker())?;
        let mut pulled =
            Pulled { changed: !report.batch.imported.is_empty(), warnings: report_lines(&report) };
        // The pull above worked, or this returned its error.
        match settle_numbering(&self.dir, &self.config, service, true) {
            Ok(moved) => pulled.warnings.extend(moved),
            Err(e) => pulled.warnings.push(format!("numbering new cards: {}", e.message)),
        }
        if let Err(e) = settle_member(&self.config, service) {
            pulled.warnings.push(format!("recording this machine as a member: {}", e.message));
        }
        if service.numbering() {
            match service.assign_numbers(&self.user) {
                Ok(numbered) => pulled.changed |= !numbered.is_empty(),
                Err(e) => pulled.warnings.push(format!("numbering new cards: {}", e.message)),
            }
        }
        pulled.warnings.extend(refresh_copy(&self.dir, &self.team, service));
        Ok(pulled)
    }

    fn push(&self, service: &RoduService<AnyStore>) -> Result<Vec<String>> {
        let Some(store) = service.store.team() else { return Ok(Vec::new()) };
        let mut warnings = team_folder(&self.dir, &self.team)?.push(store)?.warnings;
        warnings.extend(refresh_copy(&self.dir, &self.team, service));
        Ok(warnings)
    }
}

/// `rodu sync`: a pull and a push by hand, saying what happened.
pub(crate) fn sync(io: &mut Io<'_>) -> Result<()> {
    let ws = open(io, false)?;
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return Err(RoduError::invalid("This is not a team workspace")
            .with_hint("Make it one: rodu team create --folder <shared folder> --no-encrypt"));
    };
    // Like every other command, a sync by hand keeps working on what this machine has when the
    // folder cannot be reached, and says so.
    let folder = match team_folder(&ws.dir, team) {
        Ok(folder) => folder,
        Err(e) => {
            warn(io, &e.message);
            if let Some(hint) = &e.hint {
                (io.err)(&format!("hint: {hint}"));
            }
            (io.out)("Nothing was synced");
            return Ok(());
        }
    };
    let report = match folder.pull(store, &checker()) {
        Ok(report) => report,
        Err(e) => {
            warn(io, &format!("{} (working offline)", e.message));
            (io.out)("Could not reach the team folder; nothing was synced");
            return Ok(());
        }
    };
    warn_report(io, &report);
    if let Some(moved) = settle_numbering(&ws.dir, &ws.config, &ws.service, true)? {
        warn(io, &moved);
    }
    settle_member(&ws.config, &ws.service)?;
    let numbered = if ws.service.numbering() {
        ws.service.assign_numbers(&user_actor(&ws.config))?.len()
    } else {
        0
    };
    let sent = match folder.push(store) {
        Ok(pushed) => {
            pushed.warnings.iter().for_each(|line| warn(io, line));
            if pushed.written.is_some() { "sent your changes" } else { "nothing new to send" }
        }
        Err(e) => {
            warn(io, &format!("{} (your changes stay here and go out next time)", e.message));
            "could not send your changes"
        }
    };
    refresh_copy(&ws.dir, team, &ws.service).iter().for_each(|line| warn(io, line));
    (io.out)(&format!(
        "Imported {} file(s), {} still arriving; numbered {numbered} card(s); {sent}",
        report.batch.imported.len(),
        report.incomplete.len(),
    ));
    if !report.batch.waiting.is_empty() {
        (io.out)(&format!(
            "{} file(s) wait on changes from another machine that have not arrived, or on a \
             teammate's file that could not be checked yet; they are read again next time",
            report.batch.waiting.len()
        ));
    }
    Ok(())
}

/// `rodu team take-numbering --yes`: makes this machine the one that numbers the team's cards,
/// for when the machine that did is gone for good. Two machines numbering at once would give
/// out the same numbers, so it asks for `--yes`; the document names one machine, and the other
/// stops once it syncs.
pub(crate) fn take_numbering(io: &mut Io<'_>, args: &Args) -> Result<()> {
    let ws = open(io, false)?;
    let Some(store) = ws.service.store.team() else {
        return Err(RoduError::invalid("This is not a team workspace")
            .with_hint("Outside a team every card gets its number at once"));
    };
    if !args.flag("yes") {
        return Err(RoduError::invalid("Taking over card numbering needs --yes").with_hint(
            "Do it only when the machine that numbers cards is gone for good: if it comes back, \
             a card it numbered offline can get a new number. Then run: \
             rodu team take-numbering --yes",
        ));
    }
    let pulled = pull_folder(io, &ws);
    let previous = store.numbering_peer()?;
    // Asked of the document (or, for a team made before it named one, the config) without
    // writing anything, so a machine that already numbers is left exactly as it was.
    let already = previous.map_or(ws.service.numbering(), |peer| peer == store.peer());
    if already {
        (io.out)("This machine already numbers the team's cards");
        return Ok(());
    }
    store.set_numbering_peer(store.peer())?;
    settle_numbering(&ws.dir, &ws.config, &ws.service, pulled)?;
    let numbered = ws.service.assign_numbers(&user_actor(&ws.config))?.len();
    after(io, &ws);
    let from = previous.map_or_else(
        || "the machine that created the team".to_owned(),
        |peer| format!("machine {peer:016x}"),
    );
    (io.out)(&format!(
        "This machine ({:016x}) now numbers the team's cards, taking over from {from}; \
         numbered {numbered} waiting card(s)",
        store.peer()
    ));
    Ok(())
}

/// `rodu team [--show-invite]`: where this workspace syncs. An encrypted team's invite code
/// holds its key, so it is shown only when asked for.
pub(crate) fn status(io: &mut Io<'_>, args: &Args) -> Result<()> {
    let ws = open(io, false)?;
    match (&ws.config.team, ws.service.store.team()) {
        (Some(team), Some(store)) => {
            // A signed team's status shows whether this machine was admitted, which only a sync
            // can tell.
            if team.signing.is_some() {
                before(io, &ws);
                after(io, &ws);
            }
            (io.out)(&format!("Team folder: {}", team.folder));
            if !team.encrypted {
                (io.out)("Encryption: off");
                (io.out)(&format!(
                    "Invite code: {}",
                    invite_code(&team.workspace_id, team.signing.as_deref(), None).as_str()
                ));
            } else if args.flag("show-invite") {
                (io.out)("Encryption: on");
                let key = load_key(&ws.dir)?
                    .ok_or_else(|| RoduError::invalid("This workspace's team key is missing"))?;
                let code = invite_code(&team.workspace_id, team.signing.as_deref(), Some(&key));
                (io.out)(&secret_line(&["Invite code: ", code.as_str()]));
                (io.out)("Keep it secret: it holds the team key.");
            } else {
                (io.out)("Encryption: on");
                (io.out)(
                    "Invite code: hidden, it holds the team key. Show it with: \
                     rodu team --show-invite",
                );
            }
            (io.out)(&format!("This machine: {:016x}", store.peer()));
            if team.signing.is_some() {
                (io.out)(&format!("Signing: on; {}", signing_state(&ws.dir, team, store)));
            }
            (io.out)(if store.readable_copy()? {
                "Readable copy: on (plain Markdown in the folder's readable/, never encrypted)"
            } else {
                "Readable copy: off"
            });
            (io.out)(&match store.numbering_peer()? {
                Some(peer) if peer == store.peer() => {
                    "Numbering: this machine gives new cards their numbers".to_owned()
                }
                Some(peer) => format!("Numbering: done by machine {peer:016x}"),
                None if team.numbering => {
                    "Numbering: this machine gives new cards their numbers".to_owned()
                }
                None => "Numbering: done by the machine that created the team".to_owned(),
            });
        }
        _ => (io.out)("Not a team workspace. Make it one: rodu team create --folder <path>"),
    }
    Ok(())
}

/// `rodu team members`: who is on the team, the agents acting for each person, and the machines
/// each one writes from, as the team document records them. Until sync files are signed (ADR
/// 0002, step 2) that is only what each machine says.
pub(crate) fn members(io: &mut Io<'_>) -> Result<()> {
    let ws = open(io, false)?;
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return Err(RoduError::invalid("This is not a team workspace")
            .with_hint("Make it one: rodu team create --folder <shared folder>"));
    };
    // The same pull every command does first, which also records this machine if it is missing.
    before(io, &ws);
    let members = store.members()?;
    let numbering = match store.numbering_peer()? {
        Some(peer) => Some(peer),
        None if team.numbering => Some(store.peer()),
        None => None,
    };
    let mut principals = ws.service.store.list_principals()?;
    principals.sort_by(|a, b| a.name.cmp(&b.name));
    // A signed team: which machines the root admitted, and which ask to join.
    let signed = match &team.signing {
        Some(_) => {
            let folder = team_folder(&ws.dir, team)?;
            Some((folder.admissions(store)?, folder.requests()?, folder.authority(store)?))
        }
        None => None,
    };
    // The role of a machine in a signed team, by the keys admitted for it.
    let role = |peer: u64| -> Option<&'static str> {
        let (admitted, _, authority) = signed.as_ref()?;
        let keys = admitted.get(&peer)?;
        if keys.contains(&authority.owner()) {
            Some("team owner")
        } else if keys.iter().any(|key| authority.is_admin(key)) {
            Some("admin")
        } else {
            None
        }
    };
    // Replica folders are named by peer id; anything else in sync/ is not a machine.
    let folders: BTreeSet<u64> = std::fs::read_dir(Path::new(&team.folder).join(SYNC_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(rodu_sync::folder::parse_peer_dir))
        .collect();
    let all: BTreeSet<u64> = members.keys().chain(&folders).copied().collect();
    let short = |peer: u64| short_peer(peer, &all);
    let mut claimed = BTreeSet::new();
    for person in principals.iter().filter(|p| p.kind == PrincipalKind::Human) {
        (io.out)(&person.name);
        let agents: Vec<&str> = principals
            .iter()
            .filter(|p| p.owner_id.as_deref() == Some(person.id.as_str()))
            .map(|p| p.name.as_str())
            .collect();
        if !agents.is_empty() {
            (io.out)(&format!("  agents: {}", agents.join(", ")));
        }
        let machines: Vec<u64> =
            members.iter().filter(|(_, id)| **id == person.id).map(|(peer, _)| *peer).collect();
        if machines.is_empty() {
            (io.out)("  no machine recorded yet");
        }
        for peer in machines {
            claimed.insert(peer);
            let waiting =
                signed.as_ref().is_some_and(|(admitted, ..)| !admitted.contains_key(&peer));
            let marks: Vec<&str> = [
                (peer == store.peer()).then_some("this machine"),
                (Some(peer) == numbering).then_some("numbers cards"),
                role(peer),
                waiting.then_some("waiting to be admitted"),
            ]
            .into_iter()
            .flatten()
            .collect();
            (io.out)(&if marks.is_empty() {
                format!("  machine {}", short(peer))
            } else {
                format!("  machine {} ({})", short(peer), marks.join(", "))
            });
        }
    }
    // A machine asking to join is listed with its request below, not as unknown.
    let asking_peers: BTreeSet<u64> =
        signed.iter().flat_map(|(_, requests, _)| requests.iter().map(|r| r.peer)).collect();
    let unknown: Vec<u64> =
        folders.difference(&claimed).filter(|p| !asking_peers.contains(p)).copied().collect();
    if !unknown.is_empty() {
        (io.out)(
            "Unknown machines (writing to the folder, but no person on the team claims them):",
        );
        for peer in unknown {
            let waiting =
                signed.as_ref().is_some_and(|(admitted, ..)| !admitted.contains_key(&peer));
            (io.out)(&if waiting {
                format!("  machine {} (waiting to be admitted)", short(peer))
            } else {
                format!("  machine {}", short(peer))
            });
        }
    }
    match &signed {
        Some((admitted, requests, _)) => {
            let asking: Vec<&JoinRequest> =
                requests.iter().filter(|r| !is_admitted(admitted, r.peer, &r.key)).collect();
            if !asking.is_empty() {
                (io.out)(
                    "Asking to join (the team owner or an admin admits with rodu team admit):",
                );
                for request in asking {
                    (io.out)(&format!(
                        "  {} (code {}, machine {})",
                        request.name,
                        request.key.code(),
                        short(request.peer)
                    ));
                }
            }
            (io.out)(
                "The team takes in changes only from machines its owner or an admin admitted, \
                 proven by their signatures. Which person a machine belongs to is what that \
                 machine says.",
            );
        }
        None => (io.out)(
            "This list is what each machine says about itself; nothing proves it yet. Anyone who \
             can write to the team folder can add to it.",
        ),
    }
    after(io, &ws);
    Ok(())
}

/// A machine as `rodu team members` shows it: the first 8 hex digits of its peer id, which its
/// replica folder's name starts with, or all 16 when another machine listed shares those 8.
fn short_peer(peer: u64, all: &BTreeSet<u64>) -> String {
    let full = format!("{peer:016x}");
    let shared = all.iter().any(|other| *other != peer && other >> 32 == peer >> 32);
    if shared { full } else { full[..8].to_owned() }
}

/// `rodu team admit [<name> <code>]`: on the owner's or an admin's machine, lists the
/// machines asking to join, or admits one. The code is the one the person's machine printed when
/// it joined, read out by them, so a request someone planted in the folder under their name is
/// never admitted by mistake.
pub(crate) fn admit(io: &mut Io<'_>, name: Option<&String>, code: Option<&String>) -> Result<()> {
    let ws = open(io, false)?;
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return Err(RoduError::invalid("This is not a team workspace"));
    };
    if team.signing.is_none() {
        return Err(RoduError::invalid(
            "This team does not sign its files: anyone with the invite code takes part",
        )
        .with_hint("A team that admits each machine is made with: rodu team create --signed"));
    }
    before(io, &ws);
    let folder = team_folder(&ws.dir, team)?;
    let admitted = folder.admissions(store)?;
    let waiting: Vec<JoinRequest> = folder
        .requests()?
        .into_iter()
        .filter(|r| !is_admitted(&admitted, r.peer, &r.key))
        .collect();
    let (Some(name), Some(code)) = (name, code) else {
        if name.is_some() {
            return Err(RoduError::invalid(
                "Give the machine's code too: rodu team admit <name> <code>",
            )
            .with_hint("The person sees it when they join, and in: rodu team"));
        }
        if waiting.is_empty() {
            (io.out)("No machine is waiting to be admitted");
        }
        for request in &waiting {
            (io.out)(&format!(
                "{}  code {}  machine {:016x}",
                request.name,
                request.key.code(),
                request.peer
            ));
        }
        if !waiting.is_empty() {
            (io.out)(
                "Check each code with the person yourself, then: rodu team admit <name> <code>",
            );
        }
        return Ok(());
    };
    // Read out with or without its dashes.
    let digits = |code: &str| -> String {
        code.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_lowercase()).collect()
    };
    let code = code.trim();
    let matching: Vec<&JoinRequest> = waiting
        .iter()
        .filter(|r| {
            r.name == *name && digits(&r.key.code()) == digits(code) && !digits(code).is_empty()
        })
        .collect();
    let request = match matching.as_slice() {
        [request] => *request,
        [] => {
            return Err(RoduError::not_found(format!(
                "No machine asking to join as {} has code {}",
                name.escape_debug(),
                code.escape_debug()
            ))
            .with_hint(
                "rodu team admit lists the machines waiting; the code must be the one the \
                        person sees",
            ));
        }
        _ => {
            return Err(RoduError::conflict(format!(
                "More than one machine asking to join as {} has that code",
                name.escape_debug()
            )));
        }
    };
    folder.admit(store, request)?;
    after(io, &ws);
    (io.out)(&format!(
        "Admitted {}'s machine {:016x}; each machine takes in its changes when it next syncs",
        request.name, request.peer
    ));
    Ok(())
}

/// The signed team this workspace belongs to, with its folder, synced first.
fn signed_team<'a>(io: &mut Io<'_>, ws: &'a Workspace) -> Result<(&'a LoroStore, TeamFolder)> {
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return Err(RoduError::invalid("This is not a team workspace"));
    };
    if team.signing.is_none() {
        return Err(RoduError::invalid(
            "This team does not sign its files, so it has no owner or admins",
        )
        .with_hint("A team that admits each machine is made with: rodu team create --signed"));
    }
    before(io, ws);
    Ok((store, team_folder(&ws.dir, team)?))
}

/// The admitted machine of the person called `name`: their only one, or the one `machine` (its
/// peer id, or the start of it) names.
fn person_machine(
    ws: &Workspace,
    store: &LoroStore,
    folder: &TeamFolder,
    name: &str,
    machine: Option<&str>,
) -> Result<(u64, PublicKey)> {
    let person = ws
        .service
        .store
        .list_principals()?
        .into_iter()
        .find(|p| p.kind == PrincipalKind::Human && p.name == name)
        .ok_or_else(|| {
            RoduError::not_found(format!("No one on the team is called {}", name.escape_debug()))
                .with_hint("rodu team members lists the team")
        })?;
    let admitted = folder.admissions(store)?;
    let machines: Vec<(u64, PublicKey)> = store
        .members()?
        .into_iter()
        .filter(|(_, id)| *id == person.id)
        .flat_map(|(peer, _)| {
            admitted.get(&peer).into_iter().flatten().map(move |key| (peer, *key))
        })
        .filter(|(peer, _)| machine.is_none_or(|m| format!("{peer:016x}").starts_with(m)))
        .collect();
    match machines.as_slice() {
        [one] => Ok(*one),
        [] => Err(RoduError::not_found(match machine {
            Some(m) => {
                format!("{} has no admitted machine {}", name.escape_debug(), m.escape_debug())
            }
            None => format!("{} has no admitted machine", name.escape_debug()),
        })
        .with_hint("rodu team members shows each person's machines")),
        _ => Err(RoduError::conflict(format!(
            "{} has more than one admitted machine",
            name.escape_debug()
        ))
        .with_hint("Name one with --machine <id>, as rodu team members shows it")),
    }
}

/// `rodu team admin <name> on|off [--machine <id>]`: on the owner's machine, lets a person's
/// machine admit others, or stops it. Machines it admitted before stay in.
pub(crate) fn admin(
    io: &mut Io<'_>,
    args: &Args,
    name: Option<&String>,
    state: Option<&String>,
) -> Result<()> {
    let on = match state.map(String::as_str) {
        Some("on") => true,
        Some("off") => false,
        _ => return Err(RoduError::invalid("Usage: rodu team admin <name> on|off")),
    };
    let name = name.ok_or_else(|| RoduError::invalid("Usage: rodu team admin <name> on|off"))?;
    let ws = open(io, false)?;
    let (store, folder) = signed_team(io, &ws)?;
    let (peer, key) = person_machine(&ws, store, &folder, name, args.value("machine"))?;
    let authority = folder.authority(store)?;
    if authority.is_admin(&key) == on {
        (io.out)(&format!(
            "{}'s machine {peer:016x} {} an admin already",
            name,
            if on { "is" } else { "is not" }
        ));
        return Ok(());
    }
    folder.set_admin(store, &key, on)?;
    after(io, &ws);
    (io.out)(&if on {
        format!("{name}'s machine {peer:016x} can admit machines once it next syncs")
    } else {
        format!("{name}'s machine {peer:016x} can no longer admit machines; those it admitted stay")
    });
    Ok(())
}

/// `rodu team transfer-owner <name> --yes [--machine <id>]`: on the owner's machine, hands the
/// team to a person's machine. This machine stays an admin until the new owner says otherwise.
pub(crate) fn transfer_owner(io: &mut Io<'_>, args: &Args, name: Option<&String>) -> Result<()> {
    let name =
        name.ok_or_else(|| RoduError::invalid("Usage: rodu team transfer-owner <name> --yes"))?;
    let ws = open(io, false)?;
    let (store, folder) = signed_team(io, &ws)?;
    let (peer, key) = person_machine(&ws, store, &folder, name, args.value("machine"))?;
    if !args.flag("yes") {
        return Err(RoduError::invalid("Handing over the team needs --yes").with_hint(format!(
            "The new owner alone then chooses admins and can hand it on; this cannot be taken \
             back from here. Then run: rodu team transfer-owner {name} --yes"
        )));
    }
    folder.transfer(store, &key)?;
    after(io, &ws);
    (io.out)(&format!(
        "{name}'s machine {peer:016x} owns the team once it next syncs; this machine stays an admin"
    ));
    Ok(())
}

fn is_admitted(admitted: &Admitted, peer: u64, key: &PublicKey) -> bool {
    admitted.get(&peer).is_some_and(|keys| keys.contains(key))
}

/// Where this machine stands in a signed team.
fn signing_state(dir: &Path, team: &TeamConfig, store: &LoroStore) -> String {
    let Ok(Some(identity)) = load_identity(dir) else {
        return "this machine's key is missing".to_owned();
    };
    let me = identity.public();
    let code = me.code();
    let state = team_folder(dir, team)
        .and_then(|folder| Ok((folder.authority(store)?, folder.admissions(store)?)));
    match state {
        Ok((authority, _)) if authority.owner() == me => {
            format!("this machine owns the team and admits others (code {code})")
        }
        Ok((authority, _)) if authority.is_admin(&me) => {
            format!("this machine is an admin and admits others (code {code})")
        }
        Ok((_, admitted)) if is_admitted(&admitted, store.peer(), &me) => {
            format!("this machine is admitted (code {code})")
        }
        Ok(_) => format!(
            "this machine waits to be admitted (code {code}); the team owner or an admin runs: \
             rodu team admit <your name> {code}"
        ),
        Err(e) => format!("could not read the team folder ({}) (code {code})", e.message),
    }
}

fn folder_arg(io: &Io<'_>, args: &Args) -> Result<PathBuf> {
    let folder = args.value("folder").ok_or_else(|| {
        RoduError::invalid("--folder is required")
            .with_hint("The folder your team shares, e.g. in Google Drive or Dropbox")
    })?;
    Ok(resolve(&io.cwd, folder))
}

/// Whether the new team is encrypted.
fn encryption_choice(args: &Args) -> Result<bool> {
    match (args.flag("encrypt"), args.flag("no-encrypt")) {
        (true, true) => Err(RoduError::invalid("Choose one of --encrypt and --no-encrypt")),
        (true, false) => Ok(true),
        (false, true) => Ok(false),
        (false, false) => Err(RoduError::invalid("Choose --encrypt or --no-encrypt").with_hint(
            "--no-encrypt keeps the sync files readable to anyone with access to the folder",
        )),
    }
}

/// Saves the config in place: written whole to a temporary file, then renamed over it.
fn save_config(dir: &Path, config: &Config) -> Result<()> {
    let text = serde_json::to_string_pretty(config)
        .map_err(|e| RoduError::internal(format!("config: {e}")))?;
    let path = dir.join("config.json");
    let temp = dir.join("config.json.tmp");
    let _ = std::fs::remove_file(&temp);
    write_private_file(&temp, &format!("{text}\n"))
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|e| RoduError::invalid(format!("Cannot write {}: {e}", path.display())))
}

/// `rodu team create --folder <path> --encrypt|--no-encrypt`: makes this workspace a team
/// workspace.
pub(crate) fn create(io: &mut Io<'_>, args: &Args) -> Result<()> {
    let folder_path = folder_arg(io, args)?;
    let encrypted = encryption_choice(args)?;
    let signed = args.flag("signed");
    let ws = open(io, false)?;
    if let Some(team) = &ws.config.team {
        return Err(RoduError::conflict(format!(
            "This workspace already syncs through {}",
            team.folder
        )));
    }
    let dir = ws.dir.clone();
    let mut config = ws.config.clone();
    // A create that stopped half way left the document, and maybe its first file: carry on.
    let resumed = ws.service.store.team().map(LoroStore::peer);
    drop(ws);
    // A key left by an encrypted create that stopped half way is carried on with; one with a
    // plain create would mean two answers to how this workspace syncs.
    let left = load_key(&dir)?;
    let key = match (encrypted, left) {
        (true, Some(key)) => Some(key),
        (true, None) => Some(TeamKey::generate()?),
        (false, None) => None,
        (false, Some(_)) => {
            return Err(RoduError::conflict(
                "An encrypted team create stopped half way here: run it again with --encrypt",
            ));
        }
    };
    // The folder is checked before anything here changes. A folder naming a team may only be
    // taken over by the create that started it: one whose only replica folder is this one's.
    let workspace_id = if folder_path.join(TEAM_FILE).exists() {
        let info = TeamFolder::new(&folder_path).info()?;
        // Checked before a key is written: a half-made team carries on only as what it was.
        if resumed.is_some() && info.encrypted() != encrypted {
            let was = if info.encrypted() { "--encrypt" } else { "--no-encrypt" };
            return Err(RoduError::conflict(format!(
                "A team create stopped half way here: run it again with {was}"
            )));
        }
        if resumed.is_some() && info.root().is_some() != signed {
            let was = if signed { "without --signed" } else { "with --signed" };
            return Err(RoduError::conflict(format!(
                "A team create stopped half way here: run it again {was}"
            )));
        }
        match resumed {
            Some(peer) if wrote_alone(&folder_path, peer) => info.workspace_id,
            _ => {
                return Err(RoduError::conflict(format!(
                    "{} already holds another team",
                    folder_path.display()
                ))
                .with_hint("To join it instead: rodu team join <invite code> --folder <path>"));
            }
        }
    } else {
        uuidv7(now_ms())
    };
    // Written in this order, so each step finds what the one before it left: the key, the
    // document, the team file and first sync file, and the config last.
    if let Some(key) = &key
        && !dir.join(KEY_FILE).exists()
    {
        save_key(&dir, key)?;
    }
    // This machine's key is the signed team's root; one a create that stopped half way left is
    // carried on with.
    let identity = match (signed, load_identity(&dir)?) {
        (false, _) => None,
        (true, Some(identity)) => Some(identity),
        (true, None) => {
            let identity = MachineKey::generate()?;
            save_identity(&dir, &identity)?;
            Some(identity)
        }
    };
    let root = identity.as_ref().map(|identity| identity.public().to_hex());
    let code = invite_code(&workspace_id, root.as_deref(), key.as_ref());
    let store = if resumed.is_some() { LoroStore::open(&dir)? } else { LoroStore::adopt(&dir)? };
    let folder = match key {
        Some(key) => TeamFolder::sealed(&folder_path, key),
        None => TeamFolder::new(&folder_path),
    }
    .expecting(&workspace_id);
    let folder = match identity {
        Some(identity) => {
            let root = identity.public();
            folder.signed(identity, root)
        }
        None => folder,
    };
    folder.create(&workspace_id, store.peer())?;
    if let Some(root) = root.as_deref().and_then(PublicKey::from_hex) {
        // The root admits its own machine too, so every machine of a signed team is listed the
        // same way.
        let me = JoinRequest { peer: store.peer(), name: String::new(), key: root };
        folder.admit(&store, &me)?;
    }
    store.set_numbering_peer(store.peer())?;
    store.set_member(store.peer(), &config.user_id)?;
    let readable_copy = args.flag("readable-copy");
    if readable_copy {
        store.set_readable_copy(true)?;
    }
    folder.push(&store)?;
    drop(store);
    config.team = Some(TeamConfig {
        folder: folder_path.display().to_string(),
        workspace_id: workspace_id.clone(),
        numbering: true,
        encrypted,
        signing: root,
    });
    save_config(&dir, &config)?;
    (io.out)(&format!("This workspace now syncs through {}", folder_path.display()));
    (io.out)(&secret_line(&["Invite code: ", code.as_str()]));
    if encrypted {
        // The code is printed once: the join reads it from stdin, out of shell history.
        (io.out)(
            "A teammate joins with: rodu team join - --folder <the same folder on their machine> \
             --name <their name>, then pastes the invite code",
        );
        (io.out)(
            "Keep the invite code secret: it holds the team key. The sync files are encrypted; \
             anyone with the code and the folder can read and change the board.",
        );
    } else {
        (io.out)(&secret_line(&[
            "A teammate joins with: rodu team join ",
            code.as_str(),
            " --folder <the same folder on their machine> --name <their name>",
        ]));
        (io.out)(
            "The sync files are not encrypted: anyone with access to the folder can read them.",
        );
    }
    if signed {
        (io.out)(
            "Signing is on: each teammate's machine asks to join, and the team takes in its \
             changes only once this machine (the team owner) or an admin admits it with: rodu \
             team admit <name> <code>. Check the code with the teammate yourself, not through the \
             folder. Choose admins with: rodu team admin <name> on",
        );
    }
    if readable_copy {
        (io.out)(READABLE_WARNING);
        let ws = open(io, false)?;
        if let Some(team) = &ws.config.team {
            refresh_copy(&ws.dir, team, &ws.service).iter().for_each(|line| warn(io, line));
        }
    }
    Ok(())
}

/// `rodu team readable-copy on|off`: turns the team's readable copy on or off for every machine.
/// The numbering machine writes it, or removes what it wrote, when it next syncs.
pub(crate) fn readable_copy(io: &mut Io<'_>, setting: Option<&String>) -> Result<()> {
    let on = match setting.map(String::as_str) {
        Some("on") => true,
        Some("off") => false,
        _ => {
            return Err(RoduError::invalid("Say on or off")
                .with_hint("rodu team readable-copy on, or rodu team readable-copy off"));
        }
    };
    let ws = open(io, false)?;
    let Some(store) = ws.service.store.team() else {
        return Err(RoduError::invalid("This is not a team workspace")
            .with_hint("The readable copy lives in a team's shared folder"));
    };
    before(io, &ws);
    if store.readable_copy()? != on {
        store.set_readable_copy(on)?;
    }
    // after() pushes and brings the copy up to date; its problems are warnings, said above.
    after(io, &ws);
    if on {
        (io.out)(READABLE_WARNING);
    }
    (io.out)(match (on, ws.service.numbering()) {
        (true, true) => "Readable copy: on, kept by this machine",
        (true, false) => "Readable copy: on; the numbering machine writes it when it next syncs",
        (false, true) => {
            "Readable copy: off; this machine removed the files it wrote (any it kept are named \
             in the warnings above)"
        }
        (false, false) => "Readable copy: off; the numbering machine removes it when it next syncs",
    });
    Ok(())
}

/// True if `sync/` holds `peer`'s replica folder and no other.
fn wrote_alone(root: &Path, peer: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join(SYNC_DIR)) else { return false };
    let own = format!("{peer:016x}");
    let names: Vec<String> =
        entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    names.contains(&own) && names.iter().all(|n| *n == own || n.starts_with('.'))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `rodu team join <code> --folder <path> --name <you>`: a new workspace here, from the folder.
pub(crate) fn join(io: &mut Io<'_>, args: &Args, code: Option<&String>) -> Result<()> {
    // `-` reads the code from stdin, so an encrypted team's key stays out of the shell's history
    // and other users' process lists.
    let read;
    let code = match code.map(String::as_str) {
        Some("-") => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                (io.err)("Paste the invite code, then press Enter:");
            }
            let mut line = zeroize::Zeroizing::new(String::new());
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| RoduError::invalid(format!("Cannot read the invite code: {e}")))?;
            read = line;
            Some(read.as_str())
        }
        other => other,
    };
    let Invite { workspace_id, root, key } = code.and_then(parse_invite).ok_or_else(|| {
        RoduError::invalid("team join needs an invite code such as rodu1-0190…")
            .with_hint("The teammate who created the team sees it in: rodu team --show-invite")
    })?;
    let who = match (args.value("name"), args.value("as")) {
        (Some(name), None) => Joining::New(name),
        (None, Some(name)) => Joining::As(name),
        (Some(_), Some(_)) => {
            return Err(RoduError::invalid(
                "Give --name for someone new to the team, or --as for someone already on it, \
                 not both",
            ));
        }
        (None, None) => {
            return Err(RoduError::invalid("team join needs --name <you>")
                .with_hint("Joining from a second machine? Use --as <your name on the team>"));
        }
    };
    let folder_path = folder_arg(io, args)?;
    let info = TeamFolder::new(&folder_path).info()?;
    if info.workspace_id != workspace_id {
        return Err(RoduError::invalid(format!(
            "{} holds another team than this invite code",
            folder_path.display()
        )));
    }
    // The root key and the team key are checked before anything is written. The root comes from
    // the invite code, never from the folder.
    match (root, info.root()) {
        (None, None) => {}
        (Some(root), Some(named)) if root == named => {}
        (Some(_), _) => {
            return Err(RoduError::invalid("The invite code's root key is not this team's")
                .with_hint("Ask for the invite code again: rodu team --show-invite"));
        }
        (None, Some(_)) => {
            return Err(RoduError::invalid(
                "This team signs its files: the invite code needs its root key (rodu2-…)",
            )
            .with_hint("Ask for the invite code again: rodu team --show-invite"));
        }
    }
    let key_hex = key.as_ref().map(TeamKey::to_hex);
    let folder = match (info.key_check.as_deref(), key) {
        (None, None) => TeamFolder::new(&folder_path).expecting(&workspace_id),
        (Some(check), Some(key)) if key.matches(check) => {
            TeamFolder::sealed(&folder_path, key).expecting(&workspace_id)
        }
        (Some(_), Some(_)) => {
            return Err(RoduError::invalid("The invite code's key does not open this team")
                .with_hint("Ask for the invite code again: rodu team --show-invite"));
        }
        (Some(_), None) => {
            return Err(RoduError::invalid(
                "This team is encrypted: the invite code needs its key",
            )
            .with_hint("Ask for the whole code: rodu team --show-invite"));
        }
        (None, Some(_)) => {
            return Err(RoduError::invalid(
                "This team is not encrypted, but the invite code holds a key",
            ));
        }
    };
    let dir = match var(io, "RODU_DIR") {
        Some(dir) => resolve(&io.cwd, dir),
        None => io.cwd.join(".rodu"),
    };
    if dir.join("config.json").exists() || dir.join("rodu.db").exists() {
        return Err(RoduError::conflict(format!(
            "A workspace already exists at {}",
            dir.display()
        )));
    }
    // A failed join removes the key files it wrote; one that was there before is not its own.
    for file in [KEY_FILE, IDENTITY_FILE] {
        if dir.join(file).exists() {
            return Err(RoduError::conflict(format!(
                "{} is already there: move it away to join here",
                dir.join(file).display()
            )));
        }
    }
    if find_dir(io).is_some_and(|found| found != dir) {
        // Not an error: a workspace inside another one is allowed, but say which one this is.
        (io.err)(&format!("note: creating a new workspace at {}", dir.display()));
    }
    let existed = dir.exists();
    create_private_dir(&dir)
        .map_err(|e| RoduError::invalid(format!("Cannot create {}: {e}", dir.display())))?;
    let signing = match root {
        Some(root) => Some((MachineKey::generate()?, root)),
        None => None,
    };
    let result = join_into(io, &dir, folder, &folder_path, who, workspace_id, key_hex, signing);
    if result.is_err() {
        // Only what this join made, so it can simply be run again.
        let made = [
            "rodu.db",
            "rodu.db-wal",
            "rodu.db-shm",
            "rodu.loro",
            "config.json.tmp",
            KEY_FILE,
            IDENTITY_FILE,
        ];
        for file in made {
            let _ = std::fs::remove_file(dir.join(file));
        }
        if !existed {
            let _ = std::fs::remove_dir(&dir);
        }
    }
    result
}

/// Who a join is for.
#[derive(Clone, Copy)]
enum Joining<'a> {
    /// Someone new to the team, under this name.
    New(&'a str),
    /// Someone already on the team, joining from another machine.
    As(&'a str),
}

/// The person and agent a join writes for: new ones, or those of someone already on the team.
fn join_principals(service: &RoduService<AnyStore>, who: Joining<'_>) -> Result<(String, String)> {
    let name = match who {
        Joining::New(name) => {
            let user = service.create_principal(name, PrincipalKind::Human, None)?;
            let agent = service.create_principal(
                &format!("{name}-agent"),
                PrincipalKind::Agent,
                Some(&user.id),
            )?;
            return Ok((user.id, agent.id));
        }
        Joining::As(name) => name,
    };
    let mut principals = service.store.list_principals()?;
    principals.sort_by(|a, b| a.name.cmp(&b.name));
    let Some(found) = principals.iter().find(|p| p.name == name) else {
        return Err(RoduError::not_found(format!("No one called {name} is on this team"))
            .with_hint("To join as someone new: --name <you>"));
    };
    if found.kind != PrincipalKind::Human {
        return Err(RoduError::invalid(format!("{name} is an agent, not a person"))
            .with_hint("Join as the person it acts for"));
    }
    let agent = principals
        .iter()
        .find(|p| p.kind == PrincipalKind::Agent && p.owner_id.as_deref() == Some(&found.id));
    let agent = match agent {
        Some(agent) => agent.id.clone(),
        None => {
            service
                .create_principal(&format!("{name}-agent"), PrincipalKind::Agent, Some(&found.id))?
                .id
        }
    };
    Ok((found.id.clone(), agent))
}

#[allow(clippy::too_many_arguments)]
fn join_into(
    io: &mut Io<'_>,
    dir: &Path,
    folder: TeamFolder,
    folder_path: &Path,
    who: Joining<'_>,
    workspace_id: String,
    key_hex: Option<zeroize::Zeroizing<String>>,
    signing: Option<(MachineKey, PublicKey)>,
) -> Result<()> {
    if let Some(hex) = &key_hex {
        let key = TeamKey::from_hex(hex).expect("a key from the invite code");
        save_key(dir, &key)?;
    }
    let (folder, signed) = match signing {
        Some((identity, root)) => {
            save_identity(dir, &identity)?;
            let code = identity.public().code();
            (folder.signed(identity, root), Some((root.to_hex(), code)))
        }
        None => (folder, None),
    };
    let db = dir.join("rodu.db");
    write_private_file(&db, "")
        .map_err(|e| RoduError::invalid(format!("Cannot create {}: {e}", db.display())))?;
    let service =
        RoduService::new(AnyStore::Team(Box::new(LoroStore::open(dir)?))).with_numbering(false);
    let store = service.store.team().expect("a team store");
    let report = folder.pull(store, &checker())?;
    warn_report(io, &report);
    let mut collections = service.list_collections()?;
    collections.sort_by(|a, b| a.key.cmp(&b.key));
    let Some(collection) = collections.first() else {
        return Err(RoduError::not_found("The team folder holds no board yet").with_hint(
            "Wait until the folder has finished syncing to this machine, then try again",
        ));
    };
    let config = service.store.transaction(TxMode::Write, || {
        let (user_id, agent_id) = join_principals(&service, who)?;
        store.set_member(store.peer(), &user_id)?;
        Ok(Config {
            user_id,
            agent_id,
            collection: collection.key.clone(),
            team: Some(TeamConfig {
                folder: folder_path.display().to_string(),
                workspace_id,
                numbering: false,
                encrypted: key_hex.is_some(),
                signing: signed.as_ref().map(|(root, _)| root.clone()),
            }),
        })
    })?;
    folder.push(store)?;
    let (Joining::New(name) | Joining::As(name)) = who;
    if signed.is_some() {
        folder.write_request(store.peer(), name)?;
    }
    save_config(dir, &config)?;
    (io.out)(&format!("Joined the team at {} as {name}", dir.display()));
    if let Some((_, code)) = signed {
        (io.out)(&format!("This machine's code: {code}"));
        (io.out)(&format!(
            "The team does not take in your changes until its owner or an admin admits this \
             machine. Tell them your code yourself (not through the team folder) and ask them to \
             run: rodu team admit {name} {code}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(dir: &Path, numbering: bool) -> (Config, RoduService<AnyStore>) {
        let config = Config {
            user_id: "u".into(),
            agent_id: "a".into(),
            collection: "DEMO".into(),
            team: Some(TeamConfig {
                folder: dir.join("folder").display().to_string(),
                workspace_id: "w".into(),
                numbering,
                encrypted: false,
                signing: None,
            }),
        };
        let store = AnyStore::Team(Box::new(LoroStore::open(dir).unwrap()));
        (config, RoduService::new(store).with_numbering(numbering))
    }

    #[test]
    fn a_machine_is_shown_by_8_hex_digits_unless_another_shares_them() {
        let all =
            BTreeSet::from([0x3f9a_12bc_0000_0001, 0x3f9a_12bc_0000_0002, 0x0000_00ab_0000_0000]);
        assert_eq!(short_peer(0x0000_00ab_0000_0000, &all), "000000ab");
        assert_eq!(short_peer(0x3f9a_12bc_0000_0001, &all), "3f9a12bc00000001");
    }

    #[test]
    fn a_machine_missing_from_the_members_records_itself_once_its_person_is_known() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, service) = legacy(dir.path(), true);
        let store = service.store.team().unwrap();
        // The config's person is not in the document yet: nothing to record.
        settle_member(&config, &service).unwrap();
        assert!(store.members().unwrap().is_empty());

        let ann = service.create_principal("ann", PrincipalKind::Human, None).unwrap();
        config.user_id = ann.id.clone();
        settle_member(&config, &service).unwrap();
        assert_eq!(store.members().unwrap().get(&store.peer()), Some(&ann.id));

        // An entry another machine overwrote is put back.
        let eve = service.create_principal("eve", PrincipalKind::Human, None).unwrap();
        store.set_member(store.peer(), &eve.id).unwrap();
        settle_member(&config, &service).unwrap();
        assert_eq!(store.members().unwrap().get(&store.peer()), Some(&ann.id));
    }

    #[test]
    fn a_team_made_before_the_document_named_a_numbering_machine_keeps_its_creator() {
        let dir = tempfile::tempdir().unwrap();
        let (config, service) = legacy(dir.path(), true);
        // Offline, it keeps numbering but does not name itself: a hand-over may be on its way.
        assert_eq!(settle_numbering(dir.path(), &config, &service, false).unwrap(), None);
        assert_eq!(service.store.team().unwrap().numbering_peer().unwrap(), None);
        assert!(service.numbering());
        // After a pull that worked, it names itself.
        assert_eq!(settle_numbering(dir.path(), &config, &service, true).unwrap(), None);
        let store = service.store.team().unwrap();
        assert_eq!(store.numbering_peer().unwrap(), Some(store.peer()), "it names itself");
        assert!(service.numbering());
        assert!(!dir.path().join("config.json").exists(), "nothing changed to record");

        let other = tempfile::tempdir().unwrap();
        let (config, service) = legacy(other.path(), false);
        assert_eq!(settle_numbering(other.path(), &config, &service, true).unwrap(), None);
        assert_eq!(service.store.team().unwrap().numbering_peer().unwrap(), None);
        assert!(!service.numbering(), "a teammate's machine never names itself");
    }

    #[test]
    fn taking_over_on_the_machine_that_already_numbers_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".rodu");
        std::fs::create_dir_all(&dir).unwrap();
        let (config, service) = legacy(&dir, true);
        save_config(&dir, &config).unwrap();
        let before = service.store.team().unwrap().version();
        drop(service);
        let (mut out, mut err) = (Vec::<String>::new(), Vec::<String>::new());
        let mut push_out = |line: &str| out.push(line.to_owned());
        let mut push_err = |line: &str| err.push(line.to_owned());
        let mut io = Io {
            cwd: root.path().to_path_buf(),
            env: [("RODU_DIR".to_owned(), dir.display().to_string())].into(),
            out: &mut push_out,
            err: &mut push_err,
            open_url: None,
            on_web_server: None,
        };
        let args = Args { flags: vec!["yes".into()], ..Args::default() };
        take_numbering(&mut io, &args).unwrap();
        assert!(out.iter().any(|l| l.contains("already numbers")), "{out:?}");
        let store = LoroStore::open(&dir).unwrap();
        assert_eq!(store.numbering_peer().unwrap(), None);
        assert_eq!(store.version(), before, "the document is unchanged");
    }
}

//! Team workspaces (ADR 0001): `rodu team create`, `rodu team join`, `rodu sync`, and the sync
//! every command does around its work in a team workspace.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rodu_core::ids::{is_uuid, uuidv7};
use rodu_core::store::{Store, TxMode};
use rodu_core::{Actor, PrincipalKind, Result, RoduError, RoduService};
use rodu_sync::folder::{PullReport, TeamFolder};
use rodu_sync::{Checker, LoroStore};
use serde::{Deserialize, Serialize};

use crate::store::AnyStore;
use crate::{
    Args, Config, Io, Workspace, create_private_dir, find_dir, open, resolve, var,
    write_private_file,
};

/// The hidden command the import check runs in: `rodu __check-import` reads a framed import on
/// stdin, replays it, and exits 0 if it is valid, 2 if not (anything else: it crashed).
pub const CHECK_COMMAND: &str = "__check-import";
const INVITE_PREFIX: &str = "rodu1-";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamConfig {
    /// The team's sync folder, absolute.
    pub folder: String,
    pub workspace_id: String,
    /// Whether this machine is the team's numbering peer.
    pub numbering: bool,
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
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else { return };
    match TeamFolder::new(&team.folder).pull(store, &checker()) {
        Ok(report) => warn_report(io, &report),
        Err(e) => warn(io, &format!("{} (working offline)", e.message)),
    }
    if team.numbering
        && let Err(e) = ws.service.assign_numbers(&user_actor(&ws.config))
    {
        warn(io, &format!("numbering new cards: {}", e.message));
    }
}

/// After a command in a team workspace: writes this machine's new changes to the folder.
pub(crate) fn after(io: &mut Io<'_>, ws: &Workspace) {
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else { return };
    if let Err(e) = TeamFolder::new(&team.folder).push(store) {
        warn(io, &format!("{} (your changes stay here and go out next time)", e.message));
    }
}

/// After `web` or `mcp` stop: their store is gone, so the workspace is opened again to push.
pub(crate) fn after_reopen(io: &mut Io<'_>, via_agent: bool) {
    match open(io, via_agent) {
        Ok(ws) => after(io, &ws),
        Err(e) => warn(io, &e.message),
    }
}

fn warn_report(io: &mut Io<'_>, report: &PullReport) {
    for line in report.damaged.iter().chain(&report.batch.refused) {
        warn(io, &format!("skipped {line}"));
    }
    for line in report.batch.index.problems.iter().chain(&report.batch.index.conflicts) {
        warn(io, line);
    }
}

/// `rodu sync`: a pull and a push by hand, saying what happened.
pub(crate) fn sync(io: &mut Io<'_>) -> Result<()> {
    let ws = open(io, false)?;
    let (Some(team), Some(store)) = (&ws.config.team, ws.service.store.team()) else {
        return Err(RoduError::invalid("This is not a team workspace")
            .with_hint("Make it one: rodu team create --folder <shared folder> --no-encrypt"));
    };
    let folder = TeamFolder::new(&team.folder);
    let report = folder.pull(store, &checker())?;
    warn_report(io, &report);
    let numbered =
        if team.numbering { ws.service.assign_numbers(&user_actor(&ws.config))?.len() } else { 0 };
    let pushed = folder.push(store)?;
    (io.out)(&format!(
        "Imported {} file(s), {} still arriving; numbered {numbered} card(s); {}",
        report.batch.imported.len(),
        report.incomplete.len(),
        if pushed.is_some() { "sent your changes" } else { "nothing new to send" }
    ));
    Ok(())
}

/// `rodu team`: where this workspace syncs.
pub(crate) fn status(io: &mut Io<'_>) -> Result<()> {
    let ws = open(io, false)?;
    match (&ws.config.team, ws.service.store.team()) {
        (Some(team), Some(store)) => {
            (io.out)(&format!("Team folder: {}", team.folder));
            (io.out)(&format!("Invite code: {INVITE_PREFIX}{}", team.workspace_id));
            (io.out)(&format!("This machine: {:016x}", store.peer()));
            (io.out)(if team.numbering {
                "Numbering: this machine gives new cards their numbers"
            } else {
                "Numbering: done by the machine that created the team"
            });
        }
        _ => (io.out)("Not a team workspace. Make it one: rodu team create --folder <path>"),
    }
    Ok(())
}

fn folder_arg(io: &Io<'_>, args: &Args) -> Result<PathBuf> {
    let folder = args.value("folder").ok_or_else(|| {
        RoduError::invalid("--folder is required")
            .with_hint("The folder your team shares, e.g. in Google Drive or Dropbox")
    })?;
    Ok(resolve(&io.cwd, folder))
}

fn encryption_choice(args: &Args) -> Result<()> {
    match (args.flag("encrypt"), args.flag("no-encrypt")) {
        (true, true) => Err(RoduError::invalid("Choose one of --encrypt and --no-encrypt")),
        (true, false) => Err(RoduError::invalid("Encrypted teams are not available yet")
            .with_hint("Use --no-encrypt for now: the folder's provider can then read the board")),
        (false, true) => Ok(()),
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

/// `rodu team create --folder <path> --no-encrypt`: makes this workspace a team workspace.
pub(crate) fn create(io: &mut Io<'_>, args: &Args) -> Result<()> {
    let folder_path = folder_arg(io, args)?;
    encryption_choice(args)?;
    let ws = open(io, false)?;
    if let Some(team) = &ws.config.team {
        return Err(RoduError::conflict(format!(
            "This workspace already syncs through {}",
            team.folder
        )));
    }
    let dir = ws.dir.clone();
    let mut config = ws.config.clone();
    // A create that stopped half way left the document: carry on with it.
    let resumed = ws.service.store.team().is_some();
    drop(ws);
    let store = if resumed { LoroStore::open(&dir)? } else { LoroStore::adopt(&dir)? };
    let folder = TeamFolder::new(&folder_path);
    let workspace_id = match folder.info() {
        // Only a folder this replica alone has written to may be taken over, e.g. by a create
        // that stopped half way.
        Ok(info) if only_peer(&folder_path, store.peer()) => info.workspace_id,
        Ok(_) => {
            return Err(RoduError::conflict(format!(
                "{} already holds another team",
                folder_path.display()
            ))
            .with_hint("To join it instead: rodu team join <invite code> --folder <path>"));
        }
        Err(_) => uuidv7(now_ms()),
    };
    folder.create(&workspace_id)?;
    folder.push(&store)?;
    config.team = Some(TeamConfig {
        folder: folder_path.display().to_string(),
        workspace_id: workspace_id.clone(),
        numbering: true,
    });
    save_config(&dir, &config)?;
    (io.out)(&format!("This workspace now syncs through {}", folder_path.display()));
    (io.out)(&format!("Invite code: {INVITE_PREFIX}{workspace_id}"));
    (io.out)(&format!(
        "A teammate joins with: rodu team join {INVITE_PREFIX}{workspace_id} \
         --folder <the same folder on their machine> --name <their name>"
    ));
    (io.out)("The sync files are not encrypted: anyone with access to the folder can read them.");
    Ok(())
}

/// True if `sync/` holds no replica's folder but `peer`'s.
fn only_peer(root: &Path, peer: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("sync")) else { return true };
    let own = format!("{peer:016x}");
    entries.flatten().all(|e| e.file_name().to_str() == Some(own.as_str()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `rodu team join <code> --folder <path> --name <you>`: a new workspace here, from the folder.
pub(crate) fn join(io: &mut Io<'_>, args: &Args, code: Option<&String>) -> Result<()> {
    let workspace_id = code
        .and_then(|c| c.strip_prefix(INVITE_PREFIX))
        .filter(|id| is_uuid(id))
        .ok_or_else(|| {
            RoduError::invalid("team join needs an invite code such as rodu1-0190…")
                .with_hint("The teammate who created the team sees it in: rodu team")
        })?
        .to_owned();
    let name =
        args.value("name").ok_or_else(|| RoduError::invalid("team join needs --name <you>"))?;
    let folder_path = folder_arg(io, args)?;
    let folder = TeamFolder::new(&folder_path);
    let info = folder.info()?;
    if info.workspace_id != workspace_id {
        return Err(RoduError::invalid(format!(
            "{} holds another team than this invite code",
            folder_path.display()
        )));
    }
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
    if find_dir(io).is_some_and(|found| found != dir) {
        // Not an error: a workspace inside another one is allowed, but say which one this is.
        (io.err)(&format!("note: creating a new workspace at {}", dir.display()));
    }
    let existed = dir.exists();
    create_private_dir(&dir)
        .map_err(|e| RoduError::invalid(format!("Cannot create {}: {e}", dir.display())))?;
    let result = join_into(io, &dir, &folder, &folder_path, name, workspace_id);
    if result.is_err() {
        // Only what this join made, so it can simply be run again.
        for file in ["rodu.db", "rodu.db-wal", "rodu.db-shm", "rodu.loro", "config.json.tmp"] {
            let _ = std::fs::remove_file(dir.join(file));
        }
        if !existed {
            let _ = std::fs::remove_dir(&dir);
        }
    }
    result
}

fn join_into(
    io: &mut Io<'_>,
    dir: &Path,
    folder: &TeamFolder,
    folder_path: &Path,
    name: &str,
    workspace_id: String,
) -> Result<()> {
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
        let user = service.create_principal(name, PrincipalKind::Human, None)?;
        let agent = service.create_principal(
            &format!("{name}-agent"),
            PrincipalKind::Agent,
            Some(&user.id),
        )?;
        Ok(Config {
            user_id: user.id,
            agent_id: agent.id,
            collection: collection.key.clone(),
            team: Some(TeamConfig {
                folder: folder_path.display().to_string(),
                workspace_id,
                numbering: false,
            }),
        })
    })?;
    folder.push(store)?;
    save_config(dir, &config)?;
    (io.out)(&format!("Joined the team at {} as {name}", dir.display()));
    Ok(())
}

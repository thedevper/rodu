//! The `rodu` command line: workspace setup, quick item commands, the web board and MCP.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rodu_core::format::item_line;
use rodu_core::store::{Store, TxMode};
use rodu_core::{Actor, PrincipalKind, Result, RoduError, RoduService};
use rodu_http::{Bytes, RunningServer, WebServerOptions, start_web_server};
use rodu_store::SqliteStore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::store::AnyStore;
use crate::team::TeamConfig;

mod store;
pub mod team;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_files.rs"));
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_WEB_PORT: u16 = 4870;

pub const USAGE: &str = "Usage: rodu <command> [options]

  init --name <you> --key <KEY> [--title <collection name>]   create a workspace here
  add <title> [--type bug] [--priority high] [--assignee me] [--collection KEY]
  ls [query] [--limit 50]    list items (JQL-lite, e.g. \"assignee = me() ORDER BY priority\")
  show <key>                 item with its context
  mv <key> <status>          move an item, e.g. rodu mv DEMO-3 \"In Progress\"
  mcp                        serve MCP over stdio for your agent
  web [--port 4870] [--no-open]   open the kanban board in your browser (local only)
  team [--show-invite]       where this workspace syncs, and its invite code
  team create --folder <shared folder> --encrypt|--no-encrypt   share this workspace with a team
  team join <invite code|-> --folder <shared folder> --name <you>   join a team here (- reads the code from stdin)
  sync                       sync with the team folder now (every command also does)
  --version                  print the version

The workspace is the nearest .rodu directory, or $RODU_DIR.";

/// Where the CLI reads its surroundings and writes its output, so tests can run it in-process.
pub struct Io<'a> {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub out: &'a mut dyn FnMut(&str),
    pub err: &'a mut dyn FnMut(&str),
    /// Opens the board link in the user's browser; None in tests.
    pub open_url: Option<&'a mut dyn FnMut(&str)>,
    /// Takes the running web server instead of serving until Ctrl+C, so tests can stop it.
    pub on_web_server: Option<&'a mut dyn FnMut(RunningServer)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    user_id: String,
    agent_id: String,
    collection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    team: Option<TeamConfig>,
}

struct Workspace {
    dir: PathBuf,
    service: RoduService<AnyStore>,
    config: Config,
    actor: Actor,
}

// --- arguments -------------------------------------------------------------------------------

const STRING_OPTIONS: &[&str] = &[
    "name",
    "key",
    "title",
    "type",
    "priority",
    "assignee",
    "collection",
    "limit",
    "port",
    "folder",
];
const BOOL_OPTIONS: &[&str] =
    &["no-open", "version", "help", "encrypt", "no-encrypt", "show-invite"];

#[derive(Debug, Default)]
struct Args {
    values: HashMap<String, String>,
    flags: Vec<String>,
    positionals: Vec<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }
}

/// Options may appear anywhere; `--` ends them. Unknown or malformed options are errors.
fn parse_args(argv: &[String]) -> std::result::Result<Args, String> {
    let mut args = Args::default();
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            args.positionals.extend(rest.by_ref().cloned());
            break;
        }
        let (name, inline) = if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (long.to_owned(), None),
            }
        } else if let Some(short) = arg.strip_prefix('-').filter(|s| is_short_options(s)) {
            // -c KEY, -cKEY, or grouped flags such as -vh.
            let mut chars = short.chars();
            let mut flags = Vec::new();
            let mut value = None;
            while let Some(c) = chars.next() {
                match c {
                    'v' => flags.push("version"),
                    'h' => flags.push("help"),
                    'c' => {
                        let attached: String = chars.by_ref().collect();
                        value = Some(if attached.is_empty() {
                            rest.next().cloned().ok_or_else(|| {
                                "Option '-c, --collection <value>' argument missing".to_owned()
                            })?
                        } else {
                            attached
                        });
                    }
                    _ => return Err(format!("Unknown option '-{c}'")),
                }
            }
            args.flags.extend(flags.into_iter().map(str::to_owned));
            if let Some(value) = value {
                args.values.insert("collection".to_owned(), value);
            }
            continue;
        } else {
            args.positionals.push(arg.clone());
            continue;
        };
        if STRING_OPTIONS.contains(&name.as_str()) {
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("Option '--{name} <value>' argument missing"))?,
            };
            args.values.insert(name, value);
        } else if BOOL_OPTIONS.contains(&name.as_str()) {
            if inline.is_some() {
                return Err(format!("Option '--{name}' does not take an argument"));
            }
            args.flags.push(name);
        } else {
            return Err(format!("Unknown option '{arg}'"));
        }
    }
    Ok(args)
}

/// `-x...` is a cluster of short options; `-7d` (a relative date in a query) and `-` are not.
fn is_short_options(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

// --- workspace -------------------------------------------------------------------------------

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

/// An environment variable, treating an empty value as unset.
fn var<'a>(io: &'a Io<'_>, name: &str) -> Option<&'a str> {
    io.env.get(name).map(String::as_str).filter(|v| !v.is_empty())
}

fn find_dir(io: &Io<'_>) -> Option<PathBuf> {
    if let Some(dir) = var(io, "RODU_DIR") {
        return Some(resolve(&io.cwd, dir));
    }
    io.cwd.ancestors().map(|dir| dir.join(".rodu")).find(|dir| dir.join("config.json").is_file())
}

fn open(io: &Io<'_>, via_agent: bool) -> Result<Workspace> {
    let dir = find_dir(io).filter(|d| d.join("config.json").is_file()).ok_or_else(|| {
        RoduError::not_found("No Rodu workspace here").with_hint(
            "Create one in this folder: rodu init --name <you> --key <KEY> --title \"<project>\"",
        )
    })?;
    let path = dir.join("config.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| RoduError::invalid(format!("Cannot read {}: {e}", path.display())))?;
    let config: Config = serde_json::from_str(&text)
        .ok()
        .filter(|c: &Config| {
            !c.user_id.is_empty() && !c.agent_id.is_empty() && !c.collection.is_empty()
        })
        .ok_or_else(|| {
            RoduError::invalid(format!("{} is not a valid Rodu config", path.display()))
        })?;
    // A document in the folder means a team workspace, even if its config does not say so yet (a
    // team create that stopped half way): writing it as a plain one would leave the document
    // behind.
    let store = if config.team.is_some() || dir.join("rodu.loro").exists() {
        AnyStore::Team(Box::new(rodu_sync::LoroStore::open(&dir)?))
    } else {
        AnyStore::Plain(SqliteStore::open(&dir.join("rodu.db"))?)
    };
    let numbering = config.team.as_ref().is_none_or(|t| t.numbering);
    let service = RoduService::new(store).with_numbering(numbering);
    let actor = Actor {
        principal_id: config.user_id.clone(),
        via_agent_id: via_agent.then(|| config.agent_id.clone()),
    };
    Ok(Workspace { dir, service, config, actor })
}

fn init(io: &mut Io<'_>, args: &Args) -> Result<()> {
    let (Some(name), Some(key)) = (args.value("name"), args.value("key")) else {
        return Err(RoduError::invalid("init needs --name and --key")
            .with_hint("e.g. rodu init --name your-name --key DEMO"));
    };
    let dir = match var(io, "RODU_DIR") {
        Some(dir) => resolve(&io.cwd, dir),
        None => io.cwd.join(".rodu"),
    };
    let config_path = dir.join("config.json");
    if config_path.exists() {
        return Err(RoduError::conflict(format!(
            "A workspace already exists at {}",
            dir.display()
        )));
    }
    create_private_dir(&dir)
        .map_err(|e| RoduError::invalid(format!("Cannot create {}: {e}", dir.display())))?;
    // SQLite gives its -wal and -shm files the database's mode, so this keeps all three private.
    let db = dir.join("rodu.db");
    if !db.exists() {
        write_private_file(&db, "")
            .map_err(|e| RoduError::invalid(format!("Cannot create {}: {e}", db.display())))?;
    }
    let service = RoduService::new(SqliteStore::open(&db)?);
    let config = service.store.transaction(TxMode::Write, || {
        let user = service.create_principal(name, PrincipalKind::Human, None)?;
        let agent = service.create_principal(
            &format!("{name}-agent"),
            PrincipalKind::Agent,
            Some(&user.id),
        )?;
        let actor = Actor { principal_id: user.id.clone(), via_agent_id: None };
        let upper = key.to_uppercase();
        let title = args.value("title").unwrap_or(&upper);
        let collection = service.create_collection(&actor, key, title)?;
        Ok(Config { user_id: user.id, agent_id: agent.id, collection: collection.key, team: None })
    })?;
    let text = serde_json::to_string_pretty(&config)
        .map_err(|e| RoduError::internal(format!("config: {e}")))?;
    write_private_file(&config_path, &format!("{text}\n"))
        .map_err(|e| RoduError::invalid(format!("Cannot write {}: {e}", config_path.display())))?;
    (io.out)(&format!("Created {} with collection {}", dir.display(), config.collection));
    Ok(())
}

/// The workspace holds the user's data: only they may read it (no-op where there are no modes).
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn write_private_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

// --- commands --------------------------------------------------------------------------------

/// Runs one command and returns the process exit code.
pub async fn run(argv: &[String], io: &mut Io<'_>) -> i32 {
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(message) => {
            (io.err)(&format!("error: {message}\n\n{USAGE}"));
            return 1;
        }
    };
    if args.flag("version") {
        (io.out)(&format!("rodu {VERSION}"));
        return 0;
    }
    let Some(command) = args.positionals.first().cloned() else {
        (io.out)(USAGE);
        return if args.flag("help") { 0 } else { 1 };
    };
    if args.flag("help") {
        (io.out)(USAGE);
        return 0;
    }
    let rest = &args.positionals[1..];
    match command_result(&command, rest, &args, io).await {
        Ok(code) => code,
        Err(error) => {
            let hint = error.hint.map(|h| format!("\nhint: {h}")).unwrap_or_default();
            (io.err)(&format!("error: {}{hint}", error.message));
            1
        }
    }
}

async fn command_result(
    command: &str,
    rest: &[String],
    args: &Args,
    io: &mut Io<'_>,
) -> Result<i32> {
    match command {
        "init" => init(io, args).map(|()| 0),
        "web" => serve_web(io, args.value("port"), !args.flag("no-open")).await.map(|()| 0),
        "team" => match rest.first().map(String::as_str) {
            None => team::status(io, args).map(|()| 0),
            Some("create") => team::create(io, args).map(|()| 0),
            Some("join") => team::join(io, args, rest.get(1)).map(|()| 0),
            Some(other) => Err(RoduError::invalid(format!("Unknown team command \"{other}\""))
                .with_hint("rodu team, rodu team create, rodu team join")),
        },
        "sync" => team::sync(io).map(|()| 0),
        "mcp" => {
            // The agent acts for the user: every change is recorded as "alice via alice-agent".
            let ws = open(io, true)?;
            team::before(io, &ws);
            // Only MCP messages may go to stdout; the status line goes to stderr.
            (io.err)(&format!("rodu mcp: serving {} on stdio", ws.dir.display()));
            let served = rodu_mcp::serve_stdio(ws.service, ws.actor)
                .await
                .map_err(|e| RoduError::internal(format!("MCP server stopped: {e}")));
            team::after_reopen(io, true);
            served.map(|()| 0)
        }
        "add" | "ls" | "show" | "mv" => {
            let ws = open(io, false)?;
            team::before(io, &ws);
            let result = item_command(command, rest, args, &ws, io);
            team::after(io, &ws);
            result
        }
        _ => {
            (io.err)(&format!("Unknown command \"{command}\"\n\n{USAGE}"));
            Ok(1)
        }
    }
}

/// The quick item commands, on an opened workspace.
fn item_command(
    command: &str,
    rest: &[String],
    args: &Args,
    ws: &Workspace,
    io: &mut Io<'_>,
) -> Result<i32> {
    match command {
        "add" => {
            let mut item = Map::new();
            item.insert("title".into(), Value::String(rest.join(" ")));
            for field in ["type", "priority", "assignee"] {
                if let Some(value) = args.value(field) {
                    item.insert(field.into(), Value::String(value.to_owned()));
                }
            }
            let collection = args.value("collection").unwrap_or(&ws.config.collection);
            let created =
                ws.service.create_items(&ws.actor, collection, &[Value::Object(item)], None)?;
            for item in &created {
                (io.out)(&item_line(item));
            }
            Ok(0)
        }
        "ls" => {
            let limit = match args.value("limit") {
                None => 50,
                Some(raw) => {
                    raw.parse::<u32>().ok().filter(|n| (1..=100).contains(n)).ok_or_else(|| {
                        RoduError::invalid("--limit must be a whole number from 1 to 100")
                    })?
                }
            };
            let result = ws.service.search(&ws.actor, &rest.join(" "), Some(limit), None)?;
            for item in &result.items {
                (io.out)(&item_line(item));
            }
            let shown = result.items.len() as u64;
            if result.total > shown {
                (io.out)(&format!("… {} more (use --limit)", result.total - shown));
            }
            Ok(0)
        }
        "show" => {
            let key = rest.first().ok_or_else(|| RoduError::invalid("show needs an item key"))?;
            (io.out)(&ws.service.context(key, None)?);
            Ok(0)
        }
        _ => {
            let status = rest.get(1..).unwrap_or_default();
            let Some(key) = rest.first().filter(|_| !status.is_empty()) else {
                return Err(RoduError::invalid("mv needs an item key and a status"));
            };
            (io.out)(&item_line(&ws.service.transition(&ws.actor, key, &status.join(" "))?));
            Ok(0)
        }
    }
}

/// The board files: `$RODU_WEB_DIST` on disk when set, else the copy built into this binary.
enum WebSource {
    Disk(PathBuf),
    Embedded,
}

fn web_source(io: &Io<'_>) -> Result<WebSource> {
    if let Some(dist) = var(io, "RODU_WEB_DIST") {
        let dir = resolve(&io.cwd, dist);
        if !dir.join("index.html").is_file() {
            return Err(
                RoduError::not_found("The web UI is not built").with_hint("Run: cargo xtask web")
            );
        }
        return Ok(WebSource::Disk(dir));
    }
    if embedded::WEB_FILES.is_empty() {
        return Err(RoduError::not_found("The web UI is not built").with_hint(
            "This rodu was built without the board: run cargo xtask web, then build rodu again",
        ));
    }
    Ok(WebSource::Embedded)
}

async fn serve_web(io: &mut Io<'_>, port: Option<&str>, open_browser: bool) -> Result<()> {
    let source = web_source(io)?;
    let port = match port {
        None => DEFAULT_WEB_PORT,
        Some(raw) => raw
            .parse::<u16>()
            .map_err(|_| RoduError::invalid("--port must be a whole number from 0 to 65535"))?,
    };
    let ws = open(io, false)?;
    team::before(io, &ws);
    let (dist_dir, files) = match source {
        WebSource::Disk(dir) => (Some(dir), None),
        WebSource::Embedded => {
            let files = embedded::WEB_FILES
                .iter()
                .map(|(url, bytes)| ((*url).to_owned(), Bytes::from_static(bytes)))
                .collect();
            (None, Some(files))
        }
    };
    let dir = ws.dir.clone();
    let server = start_web_server(WebServerOptions {
        service: ws.service,
        actor: ws.actor,
        port,
        dist_dir,
        files,
        token: None,
    })
    .await
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            RoduError::conflict(format!("Port {port} is in use"))
                .with_hint("Pick another with --port")
        } else {
            RoduError::internal(format!("Cannot start the board: {e}"))
        }
    })?;
    (io.out)(&format!("Rodu board for {}", dir.display()));
    // The token rides in the fragment, which browsers never send to the server.
    let link = format!("{}#token={}", server.url, server.token);
    (io.out)(&format!("Open: {link}"));
    (io.out)("Local only. Press Ctrl+C to stop.");
    if open_browser && let Some(open_url) = io.open_url.as_mut() {
        open_url(&link);
    }
    match io.on_web_server.as_mut() {
        Some(take) => take(server),
        None => {
            shutdown_signal().await;
            server
                .close()
                .await
                .map_err(|e| RoduError::internal(format!("Cannot stop the board: {e}")))?;
            team::after_reopen(io, false);
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Best effort: the link is printed too, so a missing opener only costs a copy and paste.
pub fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        // The default browser via the shell, without cmd.exe and its quoting rules.
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_options_anywhere() {
        let args = parse_args(&argv(&["add", "Fix", "-c", "OPS", "it", "--type=bug"])).unwrap();
        assert_eq!(args.positionals, ["add", "Fix", "it"]);
        assert_eq!(args.value("collection"), Some("OPS"));
        assert_eq!(args.value("type"), Some("bug"));
        let args = parse_args(&argv(&["ls", "--", "--not-an-option"])).unwrap();
        assert_eq!(args.positionals, ["ls", "--not-an-option"]);
    }

    #[test]
    fn rejects_bad_options() {
        assert!(parse_args(&argv(&["--bogus"])).unwrap_err().contains("--bogus"));
        assert!(parse_args(&argv(&["init", "--name"])).unwrap_err().contains("--name"));
        assert!(parse_args(&argv(&["web", "--no-open=1"])).is_err());
        assert!(parse_args(&argv(&["-x"])).is_err());
    }
}

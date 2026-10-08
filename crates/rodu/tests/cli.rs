use std::collections::HashMap;
use std::path::Path;

use rodu::{Io, run};
use rodu_http::RunningServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Outcome {
    code: i32,
    out: Vec<String>,
    err: Vec<String>,
}

impl Outcome {
    fn last_out(&self) -> &str {
        self.out.last().map(String::as_str).unwrap_or_default()
    }

    fn last_err(&self) -> &str {
        self.err.last().map(String::as_str).unwrap_or_default()
    }
}

#[derive(Default)]
struct Extra<'a> {
    env: Vec<(&'a str, String)>,
    opened: Option<&'a mut Vec<String>>,
    servers: Option<&'a mut Vec<RunningServer>>,
}

async fn cli_with(cwd: &Path, args: &[&str], extra: Extra<'_>) -> Outcome {
    let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let env: HashMap<String, String> =
        extra.env.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut push_out = |line: &str| out.push(line.to_owned());
        let mut push_err = |line: &str| err.push(line.to_owned());
        let mut open = extra.opened.map(|opened| move |url: &str| opened.push(url.to_owned()));
        let mut take =
            extra.servers.map(|servers| move |server: RunningServer| servers.push(server));
        let mut io = Io {
            cwd: cwd.to_path_buf(),
            env,
            out: &mut push_out,
            err: &mut push_err,
            open_url: open.as_mut().map(|f| f as &mut dyn FnMut(&str)),
            on_web_server: take.as_mut().map(|f| f as &mut dyn FnMut(RunningServer)),
        };
        run(&argv, &mut io).await
    };
    Outcome { code, out, err }
}

async fn cli(cwd: &Path, args: &[&str]) -> Outcome {
    cli_with(cwd, args, Extra::default()).await
}

async fn init(cwd: &Path) {
    assert_eq!(cli(cwd, &["init", "--name", "alice", "--key", "DEMO"]).await.code, 0);
}

/// GET with the token, over raw HTTP/1.1, returning the body.
async fn get(base: &str, path: &str, token: &str) -> String {
    let host = base.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(host).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    raw.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default()
}

fn web_dist(dir: &Path) -> String {
    let dist = dir.join("dist");
    std::fs::create_dir(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<title>Rodu</title>").unwrap();
    dist.to_string_lossy().into_owned()
}

#[tokio::test]
async fn initialises_a_workspace_and_manages_items_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let made = cli(cwd, &["init", "--name", "alice", "--key", "demo"]).await;
    assert_eq!(made.code, 0);
    assert!(made.last_out().contains("with collection DEMO"));
    // Windows has no POSIX modes; the folder sits in the user's own profile there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cwd.join(".rodu/config.json")), 0o600);
        assert_eq!(mode(&cwd.join(".rodu")), 0o700);
        assert_eq!(mode(&cwd.join(".rodu/rodu.db")), 0o600);
    }
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cwd.join(".rodu/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["collection"], "DEMO");
    assert!(config["userId"].is_string() && config["agentId"].is_string());

    let added =
        cli(cwd, &["add", "Crash", "on", "login", "--type", "bug", "--assignee", "me"]).await;
    assert_eq!(added.code, 0);
    assert_eq!(added.last_out(), "DEMO-1 [Backlog] Crash on login");

    let moved = cli(cwd, &["mv", "demo-1", "In", "Progress"]).await;
    assert_eq!(moved.code, 0);
    assert_eq!(moved.last_out(), "DEMO-1 [In Progress] Crash on login");

    let listed = cli(cwd, &["ls", "assignee = me()"]).await;
    assert_eq!(listed.code, 0);
    assert_eq!(listed.out, ["DEMO-1 [In Progress] Crash on login"]);

    let shown = cli(cwd, &["show", "DEMO-1"]).await;
    assert_eq!(shown.code, 0);
    assert!(shown.last_out().contains("Crash on login"));
}

#[tokio::test]
async fn finds_the_workspace_from_a_subdirectory_or_rodu_dir() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path()).await;
    let sub = dir.path().join("a/b");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(cli(&sub, &["ls"]).await.code, 0);

    let elsewhere = tempfile::tempdir().unwrap();
    let env = vec![("RODU_DIR", dir.path().join(".rodu").to_string_lossy().into_owned())];
    let added = cli_with(elsewhere.path(), &["add", "Remote"], Extra { env, ..Extra::default() });
    assert_eq!(added.await.code, 0);
    assert_eq!(cli(dir.path(), &["ls"]).await.out, ["DEMO-1 [Backlog] Remote"]);
}

#[tokio::test]
async fn refuses_a_second_init_in_the_same_place() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path()).await;
    let again = cli(dir.path(), &["init", "--name", "bob", "--key", "OPS"]).await;
    assert_eq!(again.code, 1);
    assert!(again.last_err().contains("A workspace already exists"));
    let missing = cli(dir.path(), &["init", "--name", "bob"]).await;
    assert_eq!(missing.code, 1);
    assert!(missing.last_err().contains("hint: e.g. rodu init"));
}

#[tokio::test]
async fn prints_domain_errors_with_hints_and_a_non_zero_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let none = cli(dir.path(), &["ls"]).await;
    assert_eq!(none.code, 1);
    assert!(none.last_err().contains("hint: Create one in this folder: rodu init --name"));

    init(dir.path()).await;
    cli(dir.path(), &["add", "Unowned"]).await;
    let refused = cli(dir.path(), &["mv", "DEMO-1", "In Progress"]).await;
    assert_eq!(refused.code, 1);
    assert!(refused.last_err().contains("assign it first"));
    assert_eq!(cli(dir.path(), &["mv", "DEMO-1"]).await.code, 1);
    assert_eq!(cli(dir.path(), &["show"]).await.code, 1);
    let bad_query = cli(dir.path(), &["ls", "nosuchfield = 1"]).await;
    assert_eq!(bad_query.code, 1);
    assert!(bad_query.last_err().starts_with("error: "));
}

#[tokio::test]
async fn reports_unknown_options_and_commands_with_usage() {
    let dir = tempfile::tempdir().unwrap();
    let bogus = cli(dir.path(), &["ls", "--bogus"]).await;
    assert_eq!(bogus.code, 1);
    assert!(bogus.last_err().contains("--bogus"));
    assert!(bogus.last_err().contains("Usage: rodu"));
    let unknown = cli(dir.path(), &["frobnicate"]).await;
    assert_eq!(unknown.code, 1);
    assert!(unknown.last_err().contains("Unknown command \"frobnicate\""));
    assert_eq!(cli(dir.path(), &[]).await.code, 1);
    assert_eq!(cli(dir.path(), &["--help"]).await.code, 0);
}

#[tokio::test]
async fn limits_listings_and_rejects_a_bad_limit() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path()).await;
    for title in ["One", "Two", "Three"] {
        cli(dir.path(), &["add", title]).await;
    }
    let listed = cli(dir.path(), &["ls", "--limit", "2"]).await;
    assert_eq!(listed.out.len(), 3);
    assert_eq!(listed.last_out(), "… 1 more (use --limit)");
    for bad in ["abc", "0", "101"] {
        let res = cli(dir.path(), &["ls", "--limit", bad]).await;
        assert_eq!(res.code, 1);
        assert!(res.last_err().contains("--limit"));
    }
}

#[tokio::test]
async fn serves_the_board_with_a_token_link() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    init(cwd).await;
    let missing = Extra { env: vec![("RODU_WEB_DIST", "missing-dist".into())], ..Extra::default() };
    let res = cli_with(cwd, &["web"], missing).await;
    assert_eq!(res.code, 1);
    assert!(res.last_err().contains("cargo xtask web"));

    let dist = web_dist(cwd);
    let mut servers = Vec::new();
    let env = vec![("RODU_WEB_DIST", dist.clone())];
    let started = cli_with(
        cwd,
        &["web", "--port", "0"],
        Extra { env, servers: Some(&mut servers), ..Extra::default() },
    )
    .await;
    assert_eq!(started.code, 0);
    let link = started.out.iter().find_map(|l| l.strip_prefix("Open: ")).unwrap().to_owned();
    let (base, token) = link.split_once("#token=").unwrap();
    assert!(base.starts_with("http://127.0.0.1:") && base.ends_with('/'));
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(get(base, "/api/me", token).await, r#"{"name":"alice"}"#);
    assert!(get(base, "/", token).await.contains("<title>Rodu</title>"));

    let port = base.trim_end_matches('/').rsplit(':').next().unwrap().to_owned();
    let env = vec![("RODU_WEB_DIST", dist.clone())];
    let taken = cli_with(cwd, &["web", "--port", &port], Extra { env, ..Extra::default() }).await;
    assert_eq!(taken.code, 1);
    assert!(taken.last_err().contains(&format!("Port {port} is in use")));
    let env = vec![("RODU_WEB_DIST", dist)];
    let bad = cli_with(cwd, &["web", "--port", "abc"], Extra { env, ..Extra::default() }).await;
    assert_eq!(bad.code, 1);
    assert!(bad.last_err().contains("--port"));

    for server in servers {
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn opens_the_board_in_the_browser_unless_told_not_to() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    init(cwd).await;
    let dist = web_dist(cwd);
    let mut opened = Vec::new();
    let mut servers = Vec::new();
    for args in [&["web", "--port", "0"][..], &["web", "--port", "0", "--no-open"][..]] {
        let extra = Extra {
            env: vec![("RODU_WEB_DIST", dist.clone())],
            opened: Some(&mut opened),
            servers: Some(&mut servers),
        };
        assert_eq!(cli_with(cwd, args, extra).await.code, 0);
    }
    assert_eq!(opened.len(), 1);
    assert!(opened[0].starts_with("http://127.0.0.1:") && opened[0].contains("/#token="));
    for server in servers {
        server.close().await.unwrap();
    }
}

#[tokio::test]
async fn prints_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let res = cli(dir.path(), &["--version"]).await;
    assert_eq!(res.code, 0);
    let version = res.last_out().strip_prefix("rodu ").unwrap();
    assert_eq!(version.split('.').filter(|p| p.parse::<u32>().is_ok()).count(), 3);
}

#[tokio::test]
async fn treats_empty_variables_as_unset() {
    let dir = tempfile::tempdir().unwrap();
    let env = vec![("RODU_DIR", String::new())];
    let made = cli_with(
        dir.path(),
        &["init", "--name", "alice", "--key", "DEMO"],
        Extra { env, ..Extra::default() },
    )
    .await;
    assert_eq!(made.code, 0);
    assert!(dir.path().join(".rodu/config.json").is_file());
    assert!(!dir.path().join("config.json").exists());
    let env = vec![("RODU_DIR", String::new())];
    assert_eq!(cli_with(dir.path(), &["ls"], Extra { env, ..Extra::default() }).await.code, 0);
}

#[tokio::test]
async fn reads_short_options_with_attached_values() {
    let dir = tempfile::tempdir().unwrap();
    init(dir.path()).await;
    assert_eq!(cli(dir.path(), &["init", "--name", "bob", "--key", "OPS"]).await.code, 1);
    let env = vec![("RODU_DIR", dir.path().join("ops").to_string_lossy().into_owned())];
    let ops = Extra { env, ..Extra::default() };
    assert_eq!(cli_with(dir.path(), &["init", "--name", "bob", "--key", "OPS"], ops).await.code, 0);
    let wrong = cli(dir.path(), &["add", "Fix", "-cNOPE"]).await;
    assert_eq!(wrong.code, 1, "-cNOPE must name a collection, not join the title");
    let unknown = cli(dir.path(), &["add", "Fix", "-x"]).await;
    assert_eq!(unknown.code, 1);
    assert!(unknown.last_err().contains("-x"));
    let listed = cli(dir.path(), &["ls", "updated", ">", "-7d"]).await;
    assert_eq!(listed.code, 0, "{:?}", listed.err);
}

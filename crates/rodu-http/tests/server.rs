use std::collections::HashMap;

use axum::body::Bytes;
use rodu_core::{Actor, PrincipalKind, RoduService};
use rodu_http::{LiveOptions, RunningServer, WebServerOptions, start_web_server};
use rodu_store::SqliteStore;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "test-token-0123456789";

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON body")
    }

    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map(String::as_str).unwrap_or_default()
    }
}

#[derive(Default)]
struct Opts<'a> {
    body: Option<Value>,
    raw: Option<String>,
    headers: Vec<(&'a str, &'a str)>,
    /// A Content-Length to claim instead of the payload's real length.
    declared: Option<usize>,
}

/// Raw HTTP/1.1 so tests can send any Host or Authorization header.
async fn send(server: &RunningServer, method: &str, path: &str, opts: Opts<'_>) -> Reply {
    let payload = opts.raw.or_else(|| opts.body.map(|b| b.to_string()));
    let host = format!("127.0.0.1:{}", server.port);
    let auth = format!("Bearer {TOKEN}");
    let mut headers: Vec<(String, String)> =
        vec![("Host".into(), host), ("Authorization".into(), auth)];
    if payload.is_some() {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    for (name, value) in opts.headers {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        if !value.is_empty() {
            headers.push((name.into(), value.into()));
        }
    }
    let payload = payload.unwrap_or_default();
    let length = opts.declared.unwrap_or(payload.len());
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {length}\r\nConnection: close\r\n\r\n"));
    let mut bytes = request.into_bytes();
    bytes.extend(payload.into_bytes());

    let stream = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    // The server may answer (413) before it reads everything we send: write concurrently, and
    // keep the write half open until the reply is in (a half-closed client gets no answer).
    let write = tokio::spawn(async move {
        let _ = writer.write_all(&bytes).await;
        writer
    });
    let mut raw = Vec::new();
    let _ = reader.read_to_end(&mut raw).await;
    drop(write.await);
    parse(&raw)
}

fn parse(raw: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(raw);
    let (head, body) =
        text.split_once("\r\n\r\n").unwrap_or_else(|| panic!("a full response: {text:?}"));
    let mut lines = head.lines();
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Reply { status, headers, body: body.to_owned() }
}

fn body(value: Value) -> Opts<'static> {
    Opts { body: Some(value), ..Opts::default() }
}

fn none() -> Opts<'static> {
    Opts::default()
}

fn with<'a>(headers: Vec<(&'a str, &'a str)>) -> Opts<'a> {
    Opts { headers, ..Opts::default() }
}

fn raw<'a>(text: String, headers: Vec<(&'a str, &'a str)>) -> Opts<'a> {
    Opts { raw: Some(text), headers, ..Opts::default() }
}

fn create(title: &str) -> Opts<'static> {
    body(json!({ "collection": "DEMO", "item": { "title": title } }))
}

fn field(reply: &Reply, name: &str) -> Vec<String> {
    reply.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i[name].as_str().unwrap().to_owned())
        .collect()
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

struct Fixture {
    server: RunningServer,
    _dist: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let service = RoduService::new(SqliteStore::memory().unwrap());
    let human = service.create_principal("alice", PrincipalKind::Human, None).unwrap();
    let actor = Actor { principal_id: human.id, via_agent_id: None };
    service.create_collection(&actor, "DEMO", "Demo project").unwrap();
    service.create_collection(&actor, "OPS", "Ops").unwrap();
    service.create_items(&actor, "OPS", &[json!({ "title": "Secret ops item" })], None).unwrap();
    let dist = tempfile::tempdir().unwrap();
    std::fs::create_dir(dist.path().join("assets")).unwrap();
    std::fs::write(dist.path().join("index.html"), "<!doctype html><title>Rodu</title>").unwrap();
    std::fs::write(dist.path().join("assets/app.js"), "console.log(1)").unwrap();
    let server = start_web_server(WebServerOptions {
        service,
        actor,
        port: 0,
        dist_dir: Some(dist.path().to_path_buf()),
        files: None,
        token: Some(TOKEN.into()),
        live: None,
        team: None,
        sharing: None,
        joining: None,
    })
    .await
    .unwrap();
    Fixture { server, _dist: dist }
}

// --- security ---

#[tokio::test]
async fn binds_to_loopback_and_reports_the_url() {
    let f = fixture().await;
    assert_eq!(f.server.url, format!("http://127.0.0.1:{}/", f.server.port));
    f.server.close().await.unwrap();
}

#[tokio::test]
async fn requires_the_token_on_the_api() {
    let f = fixture().await;
    let s = &f.server;
    assert_eq!(send(s, "GET", "/api/me", with(vec![("Authorization", "")])).await.status, 401);
    let wrong = send(s, "GET", "/api/me", with(vec![("Authorization", "Bearer nope")])).await;
    assert_eq!(wrong.status, 401);
    assert_eq!(
        wrong.json(),
        json!({ "code": "unauthorized", "message": "Missing or wrong token", "hint": null })
    );
    assert_eq!(send(s, "GET", "/api/me", none()).await.json(), json!({ "name": "alice" }));
}

#[tokio::test]
async fn rejects_foreign_host_headers_even_with_the_token() {
    let f = fixture().await;
    let s = &f.server;
    let evil = format!("evil.example:{}", s.port);
    assert_eq!(send(s, "GET", "/api/me", with(vec![("Host", &evil)])).await.status, 403);
    assert_eq!(send(s, "GET", "/", with(vec![("Host", "evil.example")])).await.status, 403);
    let local = format!("localhost:{}", s.port);
    assert_eq!(send(s, "GET", "/api/me", with(vec![("Host", &local)])).await.status, 200);
}

#[tokio::test]
async fn requires_json_and_limits_body_size() {
    let f = fixture().await;
    let s = &f.server;
    let form =
        raw("collection=DEMO".into(), vec![("Content-Type", "application/x-www-form-urlencoded")]);
    assert_eq!(send(s, "POST", "/api/items", form).await.status, 415);
    // Only the headers go out: a server that closes with unread upload data makes Windows reset
    // the connection, and the reset discards the 413 the client already received.
    let big = Opts { declared: Some(1024 * 1024 + 10), ..raw(String::new(), vec![]) };
    assert_eq!(send(s, "POST", "/api/items", big).await.status, 413);
    assert_eq!(send(s, "POST", "/api/items", raw("{nope".into(), vec![])).await.status, 400);
}

#[tokio::test]
async fn serves_the_ui_with_security_headers_and_blocks_path_traversal() {
    let f = fixture().await;
    let s = &f.server;
    let page = send(s, "GET", "/", with(vec![("Authorization", "")])).await;
    assert_eq!(page.status, 200);
    assert!(page.header("content-security-policy").contains("default-src 'self'"));
    assert!(page.header("content-security-policy").contains("'wasm-unsafe-eval'"));
    assert_eq!(page.header("referrer-policy"), "no-referrer");
    assert!(send(s, "GET", "/board/DEMO", none()).await.body.contains("<title>Rodu</title>"));
    let js = send(s, "GET", "/assets/app.js", none()).await;
    assert!(js.header("content-type").contains("javascript"));
    assert_eq!(send(s, "GET", "/../package.json", none()).await.status, 404);
    // Dot segments are resolved inside the root, so this lands on the app page, never /etc/passwd.
    let traversal = send(s, "GET", "/%2e%2e/%2e%2e/etc/passwd", none()).await;
    assert!(!traversal.body.contains("root:"));
    assert!(traversal.body.contains("<title>Rodu</title>"));
    assert_eq!(send(s, "GET", "/..%5c..%5cCargo.toml", none()).await.status, 404);
    assert_eq!(send(s, "GET", "/missing.js", none()).await.status, 404);
    assert_eq!(send(s, "POST", "/", none()).await.status, 405);
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            f._dist.path().join("leak.txt"),
        )
        .unwrap();
        let leak = send(s, "GET", "/leak.txt", none()).await;
        assert_eq!(leak.status, 404);
        assert!(!leak.body.contains("top secret"));
    }
}

#[tokio::test]
async fn rejects_malformed_item_keys_as_a_bad_request() {
    let f = fixture().await;
    let res = send(&f.server, "GET", "/api/items/%E0%A4%A", none()).await;
    assert_eq!(res.status, 400);
    assert_eq!(res.json()["code"], "invalid");
}

// --- board API ---

#[tokio::test]
async fn creates_lists_moves_and_comments_on_items() {
    let f = fixture().await;
    let s = &f.server;
    for title in ["Login", "Logout", "Export"] {
        assert_eq!(send(s, "POST", "/api/items", create(title)).await.status, 201);
    }
    let board = send(s, "GET", "/api/board?collection=demo", none()).await;
    let states = board.json()["collection"]["states"].clone();
    assert!(states.as_array().unwrap().iter().any(|st| st["name"] == "In Review"));
    assert_eq!(field(&board, "key"), ["DEMO-1", "DEMO-2", "DEMO-3"]);

    let moved =
        send(s, "POST", "/api/items/DEMO-3/move", body(json!({ "before": "DEMO-1" }))).await;
    assert_eq!(moved.json()["version"], 2);
    let reordered = send(s, "GET", "/api/board?collection=DEMO", none()).await;
    assert_eq!(field(&reordered, "key"), ["DEMO-3", "DEMO-1", "DEMO-2"]);

    let comment =
        send(s, "POST", "/api/items/DEMO-1/comments", body(json!({ "body": "Looks good" }))).await;
    assert_eq!(comment.status, 201);
    let detail = send(s, "GET", "/api/items/DEMO-1", none()).await.json();
    assert_eq!(detail["comments"][0]["author"], "alice");
    assert_eq!(detail["comments"][0]["body"], "Looks good");
    assert!(detail["comments"][0]["createdAt"].is_string());
    let activity = detail["activity"].as_array().unwrap();
    assert_eq!(detail["activityTotal"], 2);
    assert_eq!(activity[0]["actor"], "alice");
    assert_eq!(activity[0]["what"], "created");
    assert_eq!(activity[1]["what"], "commented: Looks good");
    assert!(activity[1]["at"].is_string());
}

#[tokio::test]
async fn adds_a_collection_and_moves_the_revision() {
    let f = fixture().await;
    let s = &f.server;
    let before = send(s, "GET", "/api/revision", none()).await.json()["revision"].clone();
    let made =
        send(s, "POST", "/api/collections", body(json!({ "key": "web", "name": "Website" }))).await;
    assert_eq!(made.status, 201, "{}", made.body);
    assert_eq!(made.json()["key"], "WEB");
    assert_eq!(made.json()["name"], "Website");
    assert!(made.json()["states"].as_array().is_some_and(|s| !s.is_empty()));
    let after = send(s, "GET", "/api/revision", none()).await.json()["revision"].clone();
    assert_ne!(before, after, "the board learns of the new collection");

    let listed = send(s, "GET", "/api/collections", none()).await.json();
    let keys: Vec<&str> =
        listed.as_array().unwrap().iter().filter_map(|c| c["key"].as_str()).collect();
    assert_eq!(keys, ["DEMO", "OPS", "WEB"]);
    let card = body(json!({ "collection": "WEB", "item": { "title": "Landing page" } }));
    assert_eq!(send(s, "POST", "/api/items", card).await.json()["key"], "WEB-1");

    let named_by_key = send(s, "POST", "/api/collections", body(json!({ "key": "docs" }))).await;
    assert_eq!(named_by_key.status, 201, "{}", named_by_key.body);
    assert_eq!(named_by_key.json()["name"], "DOCS");

    let taken =
        send(s, "POST", "/api/collections", body(json!({ "key": "ops", "name": "x" }))).await;
    assert_eq!(taken.status, 409, "{}", taken.body);
    for bad in [
        json!({ "key": "1x" }),
        json!({ "key": "TOOLONGKEY1" }),
        json!({ "name": "no key" }),
        json!({ "key": "OK", "name": "" }),
        json!({ "key": "OK", "extra": 1 }),
        json!({ "key": "OK", "name": "n".repeat(81) }),
    ] {
        let refused = send(s, "POST", "/api/collections", body(bad.clone())).await;
        assert_eq!(refused.status, 400, "{bad}: {}", refused.body);
    }
    assert_eq!(send(s, "DELETE", "/api/collections", none()).await.status, 405);
}

#[tokio::test]
async fn maps_workflow_refusals_to_422_with_the_hint() {
    let f = fixture().await;
    let s = &f.server;
    send(s, "POST", "/api/items", create("Unowned")).await;
    let refused =
        send(s, "POST", "/api/items/DEMO-1/transition", body(json!({ "to": "In Progress" }))).await;
    assert_eq!(refused.status, 422);
    assert_eq!(refused.json()["code"], "rule_violation");
    assert!(refused.json()["hint"].as_str().unwrap().contains("assign it first"));

    let patch = json!({ "patch": { "assignee": "me" }, "expectedVersion": 1 });
    assert_eq!(send(s, "PATCH", "/api/items/DEMO-1", body(patch)).await.status, 200);
    let stale = json!({ "patch": { "title": "X" }, "expectedVersion": 1 });
    assert_eq!(send(s, "PATCH", "/api/items/DEMO-1", body(stale)).await.status, 409);
    let ok =
        send(s, "POST", "/api/items/DEMO-1/transition", body(json!({ "to": "In Progress" }))).await;
    assert_eq!(ok.json()["status"], "In Progress");
}

#[tokio::test]
async fn creates_a_card_straight_into_a_column_or_not_at_all() {
    let f = fixture().await;
    let s = &f.server;
    let todo = json!({ "collection": "DEMO", "item": { "title": "Planned" }, "status": "Todo" });
    let todo = send(s, "POST", "/api/items", body(todo)).await;
    assert_eq!(todo.status, 201);
    assert_eq!(todo.json()["key"], "DEMO-1");
    assert_eq!(todo.json()["status"], "Todo");

    let refused =
        json!({ "collection": "DEMO", "item": { "title": "Unowned" }, "status": "In Progress" });
    assert_eq!(send(s, "POST", "/api/items", body(refused)).await.status, 422);
    let board = send(s, "GET", "/api/board?collection=DEMO", none()).await;
    assert_eq!(field(&board, "title"), ["Planned"]);
}

#[tokio::test]
async fn answers_405_for_a_wrong_method_and_404_before_reading_a_body() {
    let f = fixture().await;
    let s = &f.server;
    let wrong = send(s, "DELETE", "/api/items/DEMO-1", none()).await;
    assert_eq!(wrong.status, 405);
    assert_eq!(wrong.json()["code"], "invalid");
    assert_eq!(send(s, "PUT", "/api/board?collection=DEMO", none()).await.status, 405);
    let unknown = raw("x".into(), vec![("Content-Type", "text/plain")]);
    assert_eq!(send(s, "POST", "/api/nope", unknown).await.status, 404);
}

#[tokio::test]
async fn scopes_the_board_filter_to_the_collection() {
    let f = fixture().await;
    let s = &f.server;
    send(s, "POST", "/api/items", create("Mine")).await;
    let board = |filter: &str| format!("/api/board?collection=DEMO&q={}", encode(filter));
    let breakout = send(s, "GET", &board(r#"title ~ "x") OR (title ~ "Secret""#), none()).await;
    assert_eq!(breakout.status, 400);
    assert_eq!(send(s, "GET", &board("ORDER BY title"), none()).await.status, 400);
    let filtered = send(s, "GET", &board(r#"title ~ "zzz""#), none()).await;
    assert!(field(&filtered, "title").is_empty());
    let all = send(s, "GET", &board("type = task"), none()).await;
    assert_eq!(field(&all, "key"), ["DEMO-1"]);
    let spaced = send(s, "GET", "/api/board?collection=DEMO&q=type+%3D+task", none()).await;
    assert_eq!(field(&spaced, "key"), ["DEMO-1"]);
    assert_eq!(send(s, "GET", "/api/board", none()).await.status, 400);
}

#[tokio::test]
async fn moves_a_card_into_a_column_at_a_position_in_one_step_or_not_at_all() {
    let f = fixture().await;
    let s = &f.server;
    for title in ["A", "B", "C"] {
        let item = json!({ "collection": "DEMO", "item": { "title": title, "assignee": "me" } });
        send(s, "POST", "/api/items", body(item)).await;
    }
    let to = || body(json!({ "to": "In Progress" }));
    send(s, "POST", "/api/items/DEMO-1/transition", to()).await;
    send(s, "POST", "/api/items/DEMO-2/transition", to()).await;
    let placed = json!({ "to": "In Progress", "before": "DEMO-1" });
    assert_eq!(send(s, "POST", "/api/items/DEMO-3/transition", body(placed)).await.status, 200);
    let board = send(s, "GET", "/api/board?collection=DEMO", none()).await.json();
    let in_progress: Vec<&str> = board["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["status"] == "In Progress")
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert_eq!(in_progress, ["DEMO-3", "DEMO-1", "DEMO-2"]);

    let item = json!({ "collection": "DEMO", "item": { "title": "D", "assignee": "me" } });
    send(s, "POST", "/api/items", body(item)).await;
    let across = json!({ "to": "In Progress", "before": "OPS-1" });
    assert_eq!(send(s, "POST", "/api/items/DEMO-4/transition", body(across)).await.status, 400);
    let after = send(s, "GET", "/api/items/DEMO-4", none()).await.json();
    assert_eq!(after["item"]["status"], "Backlog");
}

#[tokio::test]
async fn reports_filter_errors_against_the_users_own_filter() {
    let f = fixture().await;
    let path = format!("/api/board?collection=DEMO&q={}", encode("nosuchfield = 1"));
    let res = send(&f.server, "GET", &path, none()).await;
    assert_eq!(res.status, 400);
    assert!(res.json()["message"].as_str().unwrap().contains("Query error at 0"));
    assert!(!res.json()["hint"].as_str().unwrap_or_default().contains(r#"collection = "DEMO""#));
}

#[tokio::test]
async fn validates_request_bodies_before_the_service() {
    let f = fixture().await;
    let s = &f.server;
    let extra = body(json!({ "to": "", "extra": 1 }));
    let res = send(s, "POST", "/api/items/DEMO-1/transition", extra).await;
    assert_eq!(res.status, 400);
    assert!(res.json()["message"].as_str().unwrap().contains("Invalid request"));
    let empty = send(s, "POST", "/api/items/DEMO-1/transition", body(json!({ "to": "" }))).await;
    assert_eq!(empty.status, 400);
    assert!(empty.json()["message"].as_str().unwrap().contains("Invalid request"));
    assert_eq!(send(s, "DELETE", "/api/items/DEMO-1", none()).await.status, 405);
}

#[tokio::test]
async fn lists_people_and_collections_and_answers_404_for_unknown_items() {
    let f = fixture().await;
    let s = &f.server;
    let people = send(s, "GET", "/api/principals", none()).await.json();
    assert_eq!(people, json!([{ "name": "alice", "kind": "human" }]));
    let collections = send(s, "GET", "/api/collections", none()).await.json();
    let keys: Vec<&str> =
        collections.as_array().unwrap().iter().map(|c| c["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["DEMO", "OPS"]);
    assert_eq!(send(s, "GET", "/api/items/DEMO-99", none()).await.status, 404);
}

// --- embedded UI ---

#[tokio::test]
async fn serves_files_held_in_memory_as_the_single_binary_does() {
    let service = RoduService::new(SqliteStore::memory().unwrap());
    let human = service.create_principal("bob", PrincipalKind::Human, None).unwrap();
    let files = HashMap::from([
        ("/index.html".to_owned(), Bytes::from_static(b"<!doctype html><title>Embedded</title>")),
        ("/assets/app.js".to_owned(), Bytes::from_static(b"console.log(2)")),
        ("/assets/app.wasm".to_owned(), Bytes::from_static(b"\0asm")),
    ]);
    let server = start_web_server(WebServerOptions {
        service,
        actor: Actor { principal_id: human.id, via_agent_id: None },
        port: 0,
        dist_dir: None,
        files: Some(files),
        token: Some(TOKEN.into()),
        live: None,
        team: None,
        sharing: None,
        joining: None,
    })
    .await
    .unwrap();
    let s = &server;
    let page = send(s, "GET", "/", none()).await;
    assert!(page.body.contains("<title>Embedded</title>"));
    assert_eq!(page.header("cache-control"), "no-store");
    let js = send(s, "GET", "/assets/app.js", none()).await;
    assert_eq!(js.body, "console.log(2)");
    assert!(js.header("content-type").contains("javascript"));
    // Built files keep fixed names, so the browser must revalidate them after a rebuild.
    assert_eq!(js.header("cache-control"), "no-cache");
    let wasm = send(s, "GET", "/assets/app.wasm", none()).await;
    assert_eq!(wasm.header("content-type"), "application/wasm");
    assert!(send(s, "GET", "/board/route", none()).await.body.contains("Embedded"));
    assert_eq!(send(s, "GET", "/missing.js", none()).await.status, 404);
    assert!(!send(s, "GET", "/%2e%2e/%2e%2e/etc/passwd", none()).await.body.contains("root:"));
    server.close().await.unwrap();
}

#[tokio::test]
async fn without_a_ui_serves_only_the_api() {
    let service = RoduService::new(SqliteStore::memory().unwrap());
    let human = service.create_principal("carol", PrincipalKind::Human, None).unwrap();
    let server = start_web_server(WebServerOptions {
        service,
        actor: Actor { principal_id: human.id, via_agent_id: None },
        port: 0,
        dist_dir: None,
        files: None,
        token: None,
        live: None,
        team: None,
        sharing: None,
        joining: None,
    })
    .await
    .unwrap();
    assert_eq!(server.token.len(), 64);
    let page = send(&server, "GET", "/", none()).await;
    assert_eq!(page.status, 404);
    assert_eq!(page.json()["message"], "The web UI is not built");
}

#[tokio::test]
async fn closes_even_when_a_client_stalls_mid_body() {
    let f = fixture().await;
    let port = f.server.port;
    let mut stalled = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!(
        "POST /api/items HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    stalled.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    f.server.close().await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(8), "{:?}", started.elapsed());
}

// --- live sync ---

/// Counts its calls; reports a change while `changed` is set.
#[derive(Default)]
struct FakeSync {
    pulls: std::sync::atomic::AtomicUsize,
    pushes: std::sync::atomic::AtomicUsize,
    changed: std::sync::atomic::AtomicBool,
}

impl rodu_core::LiveSync<SqliteStore> for FakeSync {
    fn pull(&self, _: &RoduService<SqliteStore>) -> rodu_core::Result<rodu_core::Pulled> {
        use std::sync::atomic::Ordering::SeqCst;
        self.pulls.fetch_add(1, SeqCst);
        Ok(rodu_core::Pulled { changed: self.changed.load(SeqCst), warnings: Vec::new() })
    }

    fn push(&self, _: &RoduService<SqliteStore>) -> rodu_core::Result<Vec<String>> {
        self.pushes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

async fn live_server(sync: std::sync::Arc<FakeSync>) -> RunningServer {
    let service = RoduService::new(SqliteStore::memory().unwrap());
    let human = service.create_principal("alice", PrincipalKind::Human, None).unwrap();
    let actor = Actor { principal_id: human.id, via_agent_id: None };
    service.create_collection(&actor, "DEMO", "Demo project").unwrap();
    start_web_server(WebServerOptions {
        service,
        actor,
        port: 0,
        dist_dir: None,
        files: None,
        token: Some(TOKEN.into()),
        live: Some(LiveOptions { sync, every: std::time::Duration::from_millis(40) }),
        team: None,
        sharing: None,
        joining: None,
    })
    .await
    .unwrap()
}

async fn revision(s: &RunningServer) -> u64 {
    let reply = send(s, "GET", "/api/revision", none()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()["revision"].as_u64().unwrap()
}

/// Waits up to two seconds for `done`.
async fn eventually(mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if done().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("not within two seconds");
}

#[tokio::test]
async fn a_live_server_syncs_on_a_timer_and_after_writes_and_counts_revisions() {
    use std::sync::atomic::Ordering::SeqCst;
    let sync = std::sync::Arc::new(FakeSync::default());
    let s = live_server(sync.clone()).await;
    eventually(async || sync.pulls.load(SeqCst) >= 2 && sync.pushes.load(SeqCst) >= 2).await;
    // Pulls that change nothing leave the revision alone.
    let quiet = revision(&s).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(revision(&s).await, quiet);
    // A pull that took something in raises it.
    sync.changed.store(true, SeqCst);
    eventually(async || revision(&s).await > quiet).await;
    sync.changed.store(false, SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let before = revision(&s).await;
    let pushes = sync.pushes.load(SeqCst);
    // A write raises it and is pushed at once.
    let made = send(
        &s,
        "POST",
        "/api/items",
        body(json!({ "collection": "DEMO", "item": { "title": "Live" }, "status": "Todo" })),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.body);
    assert!(revision(&s).await > before);
    assert!(sync.pushes.load(SeqCst) > pushes);
    // The revision asks for the token like every other route.
    let refused = send(&s, "GET", "/api/revision", with(vec![("Authorization", "")])).await;
    assert_eq!(refused.status, 401);
    // Closing stops the timer. A pull already running on a blocking thread cannot be cancelled
    // and may still land just after close returns: let it settle, then nothing more starts.
    s.close().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = sync.pulls.load(SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(sync.pulls.load(SeqCst), after, "a pull started after close");
}

#[tokio::test]
async fn a_server_without_live_sync_still_answers_the_revision() {
    let f = fixture().await;
    // The number means nothing by itself; the board only compares it with the last one.
    let first = revision(&f.server).await;
    assert_eq!(revision(&f.server).await, first);
    f.server.close().await.unwrap();
}

// --- the team panel ---

/// Answers every team call, recording each as `<call> <args>`.
#[derive(Default)]
struct FakeTeam {
    calls: Mutex<Vec<String>>,
}

use std::sync::Mutex;

impl rodu_http::TeamPanel<SqliteStore> for FakeTeam {
    fn view(&self, _: &RoduService<SqliteStore>) -> rodu_core::Result<rodu_api::TeamView> {
        self.calls.lock().unwrap().push("view".into());
        Ok(rodu_api::TeamView {
            folder: "/shared/team".into(),
            encrypted: true,
            signed: true,
            may_admit: true,
            people: Vec::new(),
            unknown: Vec::new(),
            asking: Vec::new(),
            invites: Vec::new(),
            trust: "signed".into(),
        })
    }

    fn invite(
        &self,
        _: &RoduService<SqliteStore>,
        name: &str,
        days: Option<u64>,
    ) -> rodu_core::Result<rodu_api::InviteCreated> {
        self.calls.lock().unwrap().push(format!("invite {name} {days:?}"));
        if name == "nobody" {
            return Err(rodu_core::RoduError::invalid("Only the team owner's machine can invite"));
        }
        Ok(rodu_api::InviteCreated {
            name: name.into(),
            code: "rodu2-x+y.bob".into(),
            until: "2026-01-31".into(),
        })
    }

    fn revoke(
        &self,
        _: &RoduService<SqliteStore>,
        name: &str,
    ) -> rodu_core::Result<rodu_api::TeamDone> {
        self.calls.lock().unwrap().push(format!("revoke {name}"));
        Ok(rodu_api::TeamDone { message: "cancelled".into() })
    }

    fn admit(
        &self,
        _: &RoduService<SqliteStore>,
        name: &str,
        code: &str,
    ) -> rodu_core::Result<rodu_api::TeamDone> {
        self.calls.lock().unwrap().push(format!("admit {name} {code}"));
        Ok(rodu_api::TeamDone { message: "admitted".into() })
    }

    fn remove(
        &self,
        _: &RoduService<SqliteStore>,
        name: &str,
        machine: Option<&str>,
    ) -> rodu_core::Result<rodu_api::TeamDone> {
        self.calls.lock().unwrap().push(format!("remove {name} {machine:?}"));
        Ok(rodu_api::TeamDone { message: "removed".into() })
    }
}

async fn team_server(
    team: Option<std::sync::Arc<FakeTeam>>,
    sync: std::sync::Arc<FakeSync>,
) -> RunningServer {
    sharing_server(team, None, sync).await
}

async fn sharing_server(
    team: Option<std::sync::Arc<FakeTeam>>,
    sharing: Option<std::sync::Arc<FakeSharing>>,
    sync: std::sync::Arc<FakeSync>,
) -> RunningServer {
    let service = RoduService::new(SqliteStore::memory().unwrap());
    let human = service.create_principal("alice", PrincipalKind::Human, None).unwrap();
    let actor = Actor { principal_id: human.id, via_agent_id: None };
    start_web_server(WebServerOptions {
        service,
        actor,
        port: 0,
        dist_dir: None,
        files: None,
        token: Some(TOKEN.into()),
        // The timer stays out of the way: only the requests sync.
        live: Some(LiveOptions { sync, every: std::time::Duration::from_secs(3600) }),
        team: team.map(|t| t as std::sync::Arc<dyn rodu_http::TeamPanel<SqliteStore>>),
        sharing: sharing.map(|t| t as std::sync::Arc<dyn rodu_http::Sharing<SqliteStore>>),
        joining: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn the_team_routes_reach_the_panel_and_a_change_pulls_first_and_pushes_after() {
    use std::sync::atomic::Ordering::SeqCst;
    let team = std::sync::Arc::new(FakeTeam::default());
    let sync = std::sync::Arc::new(FakeSync::default());
    let s = team_server(Some(team.clone()), sync.clone()).await;
    // The timer's first tick runs at once; wait for it, then count from there.
    eventually(async || sync.pushes.load(SeqCst) >= 1).await;
    let (pulls, pushes) = (sync.pulls.load(SeqCst), sync.pushes.load(SeqCst));

    let reply = send(&s, "GET", "/api/team", none()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["mayAdmit"], true);
    assert_eq!((sync.pulls.load(SeqCst), sync.pushes.load(SeqCst)), (pulls, pushes), "a look");

    let body = |v: Value| Opts { body: Some(v), ..Opts::default() };
    let reply = send(&s, "POST", "/api/team/invites", body(json!({"name": "bob"}))).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.json()["code"], "rodu2-x+y.bob");
    assert_eq!((sync.pulls.load(SeqCst), sync.pushes.load(SeqCst)), (pulls + 1, pushes + 1));
    for (path, payload) in [
        ("/api/team/invites/revoke", json!({"name": "bob"})),
        ("/api/team/admit", json!({"name": "cat", "code": "1234"})),
        ("/api/team/remove", json!({"name": "cat", "machine": "0f19"})),
    ] {
        let reply = send(&s, "POST", path, body(payload)).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
    }
    assert_eq!(
        *team.calls.lock().unwrap(),
        ["view", "invite bob None", "revoke bob", "admit cat 1234", "remove cat Some(\"0f19\")"]
    );
    // A refused change is the panel's error. It is pushed all the same, since a team change can
    // write before it fails.
    let pushes = sync.pushes.load(SeqCst);
    let reply = send(&s, "POST", "/api/team/invites", body(json!({"name": "nobody"}))).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.json()["message"].as_str().unwrap().contains("Only the team owner"));
    assert_eq!(sync.pushes.load(SeqCst), pushes + 1);
}

#[tokio::test]
async fn the_team_routes_need_the_token_the_right_method_and_a_known_body() {
    let team = std::sync::Arc::new(FakeTeam::default());
    let s = team_server(Some(team.clone()), std::sync::Arc::default()).await;
    let no_token = Opts { headers: vec![("Authorization", "")], ..Opts::default() };
    assert_eq!(send(&s, "GET", "/api/team", no_token).await.status, 401);
    assert_eq!(send(&s, "GET", "/api/team/invites", none()).await.status, 405);
    assert_eq!(send(&s, "POST", "/api/team", none()).await.status, 405);
    assert_eq!(send(&s, "POST", "/api/team/other", none()).await.status, 404);
    let extra = Opts { body: Some(json!({"name": "bob", "admin": true})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/team/invites", extra).await.status, 400);
    let wrong = Opts { body: Some(json!({"name": 7})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/team/remove", wrong).await.status, 400);
    assert!(team.calls.lock().unwrap().is_empty(), "nothing reached the panel");
}

#[tokio::test]
async fn a_workspace_with_no_team_says_so() {
    let s = team_server(None, std::sync::Arc::default()).await;
    let reply = send(&s, "GET", "/api/team", none()).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(reply.json()["hint"].as_str().unwrap().contains("rodu team create"));
    let body = Opts { body: Some(json!({"name": "bob"})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/team/invites", body).await.status, 404);
}

#[tokio::test]
async fn the_revision_counts_writes_another_process_makes_to_the_same_workspace() {
    // `rodu mcp` or a CLI command writes to the same database while the board runs (#26).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rodu.db");
    let service = RoduService::new(SqliteStore::open(&path).unwrap());
    let human = service.create_principal("alice", PrincipalKind::Human, None).unwrap();
    let actor = Actor { principal_id: human.id, via_agent_id: None };
    service.create_collection(&actor, "DEMO", "Demo project").unwrap();
    let s = start_web_server(WebServerOptions {
        service,
        actor: actor.clone(),
        port: 0,
        dist_dir: None,
        files: None,
        token: Some(TOKEN.into()),
        live: None,
        team: None,
        sharing: None,
        joining: None,
    })
    .await
    .unwrap();
    let before = revision(&s).await;
    assert_eq!(revision(&s).await, before, "nothing changed, so neither does the revision");

    let other = RoduService::new(SqliteStore::open(&path).unwrap());
    other.create_items(&actor, "DEMO", &[json!({ "title": "Made elsewhere" })], None).unwrap();
    assert_ne!(revision(&s).await, before);

    // The server's own writes still count, and a quiet moment after them changes nothing.
    let after = revision(&s).await;
    let reply = send(&s, "POST", "/api/items", create("Made here")).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let mine = revision(&s).await;
    assert_ne!(mine, after);
    assert_eq!(revision(&s).await, mine);
}

// --- sharing a plain board ---

/// One place, `/drive/a`. A share there works; `/drive/half` fails having changed the
/// workspace; any other path is refused with nothing changed.
#[derive(Default)]
struct FakeSharing {
    calls: Mutex<Vec<String>>,
    /// How long looking for places takes, as on a slow Drive mount.
    places_take: std::time::Duration,
}

impl rodu_http::Sharing<SqliteStore> for FakeSharing {
    fn places(&self) -> rodu_core::Result<Vec<rodu_api::PlaceView>> {
        self.calls.lock().unwrap().push("places".into());
        std::thread::sleep(self.places_take);
        Ok(vec![rodu_api::PlaceView {
            label: "My Drive (ann@example.com)".into(),
            path: "/drive/a".into(),
            shared: false,
        }])
    }

    fn share(
        &self,
        _: &RoduService<SqliteStore>,
        path: &str,
    ) -> (rodu_core::Result<rodu_api::TeamDone>, bool) {
        self.calls.lock().unwrap().push(format!("share {path}"));
        match path {
            "/drive/a" => (Ok(rodu_api::TeamDone { message: "shared".into() }), true),
            "/drive/half" => (Err(rodu_core::RoduError::invalid("stopped half way")), true),
            "/drive/slow" => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                (Ok(rodu_api::TeamDone { message: "shared".into() }), true)
            }
            _ => (Err(rodu_core::RoduError::invalid("not a place")), false),
        }
    }
}

/// True if the server asked to be reopened within a short wait.
async fn reopen_asked(s: &RunningServer) -> bool {
    tokio::time::timeout(std::time::Duration::from_millis(500), s.reopen_wanted()).await.is_ok()
}

fn share_body(path: &str) -> Opts<'static> {
    Opts { body: Some(json!({ "path": path })), ..Opts::default() }
}

#[tokio::test]
async fn a_share_from_the_board_answers_then_stops_serving_and_asks_to_reopen() {
    let sharing = std::sync::Arc::new(FakeSharing::default());
    let s = sharing_server(None, Some(sharing.clone()), std::sync::Arc::default()).await;
    let reply = send(&s, "GET", "/api/team/places", none()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()[0]["path"], "/drive/a");
    let reply = send(&s, "POST", "/api/team/share", share_body("/drive/a")).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["message"], "shared");
    assert!(reopen_asked(&s).await);
    // The store it holds is no longer the workspace's: nothing is read or written through it.
    for (method, path) in [("GET", "/api/me"), ("GET", "/api/team/places")] {
        let reply = send(&s, method, path, none()).await;
        assert_eq!(reply.status, 503, "{path}: {}", reply.body);
        assert_eq!(reply.json()["code"], "reopening");
    }
    let made = Opts { body: Some(json!({"title": "lost"})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/items", made).await.status, 503);
    assert_eq!(*sharing.calls.lock().unwrap(), ["places", "share /drive/a"]);
}

#[tokio::test]
async fn a_share_that_stopped_half_way_still_reopens_and_one_that_changed_nothing_does_not() {
    let sharing = std::sync::Arc::new(FakeSharing::default());
    let s = sharing_server(None, Some(sharing.clone()), std::sync::Arc::default()).await;
    let reply = send(&s, "POST", "/api/team/share", share_body("/elsewhere")).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(!reopen_asked(&s).await);
    assert_eq!(send(&s, "GET", "/api/me", none()).await.status, 200, "still serving");

    let reply = send(&s, "POST", "/api/team/share", share_body("/drive/half")).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(reply.json()["message"].as_str().unwrap().contains("half way"));
    assert!(reopen_asked(&s).await);
    assert_eq!(send(&s, "GET", "/api/me", none()).await.status, 503);
}

#[tokio::test]
async fn the_share_routes_need_the_token_the_right_method_and_a_known_body() {
    let sharing = std::sync::Arc::new(FakeSharing::default());
    let s = sharing_server(None, Some(sharing.clone()), std::sync::Arc::default()).await;
    let no_token = Opts { headers: vec![("Authorization", "")], ..Opts::default() };
    assert_eq!(send(&s, "GET", "/api/team/places", no_token).await.status, 401);
    let no_token = Opts { headers: vec![("Authorization", "")], ..share_body("/drive/a") };
    assert_eq!(send(&s, "POST", "/api/team/share", no_token).await.status, 401);
    assert_eq!(send(&s, "GET", "/api/team/share", none()).await.status, 405);
    assert_eq!(send(&s, "POST", "/api/team/places", none()).await.status, 405);
    let extra =
        Opts { body: Some(json!({"path": "/drive/a", "encrypt": false})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/team/share", extra).await.status, 400);
    let wrong = Opts { body: Some(json!({"path": 7})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/team/share", wrong).await.status, 400);
    assert!(sharing.calls.lock().unwrap().is_empty(), "nothing reached the sharing");
    assert!(!reopen_asked(&s).await);
}

#[tokio::test]
async fn a_team_board_is_not_shared_again_and_one_without_sharing_says_so() {
    let sharing = std::sync::Arc::new(FakeSharing::default());
    let team = std::sync::Arc::new(FakeTeam::default());
    let s = sharing_server(Some(team), Some(sharing.clone()), std::sync::Arc::default()).await;
    assert_eq!(send(&s, "GET", "/api/team/places", none()).await.status, 409);
    assert_eq!(send(&s, "POST", "/api/team/share", share_body("/drive/a")).await.status, 409);
    assert!(sharing.calls.lock().unwrap().is_empty());

    let s = sharing_server(None, None, std::sync::Arc::default()).await;
    let reply = send(&s, "GET", "/api/team/places", none()).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(reply.json()["hint"].as_str().unwrap().contains("rodu team create"));
}

#[tokio::test]
async fn looking_for_places_does_not_hold_up_the_board() {
    let sharing = std::sync::Arc::new(FakeSharing {
        places_take: std::time::Duration::from_millis(800),
        ..FakeSharing::default()
    });
    let s =
        std::sync::Arc::new(sharing_server(None, Some(sharing), std::sync::Arc::default()).await);
    let looker = std::sync::Arc::clone(&s);
    let places =
        tokio::spawn(async move { send(&looker, "GET", "/api/team/places", none()).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let started = std::time::Instant::now();
    assert_eq!(send(&s, "GET", "/api/me", none()).await.status, 200);
    assert!(started.elapsed() < std::time::Duration::from_millis(400), "{:?}", started.elapsed());
    assert_eq!(places.await.unwrap().status, 200);
}

#[tokio::test]
async fn a_request_waiting_behind_a_share_is_refused_rather_than_served_from_the_old_store() {
    let sharing = std::sync::Arc::new(FakeSharing::default());
    let s = std::sync::Arc::new(
        sharing_server(None, Some(sharing.clone()), std::sync::Arc::default()).await,
    );
    let sharer = std::sync::Arc::clone(&s);
    let share = tokio::spawn(async move {
        send(&sharer, "POST", "/api/team/share", share_body("/drive/slow")).await
    });
    // The share holds the service lock for half a second; this request waits behind it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let made = Opts {
        body: Some(json!({"collection": "X", "item": {"title": "late"}})),
        ..Opts::default()
    };
    let late = send(&s, "POST", "/api/items", made).await;
    assert_eq!(share.await.unwrap().status, 200);
    assert_eq!(late.status, 503, "{}", late.body);
    assert_eq!(late.json()["code"], "reopening");
}

// --- joining a team where there is no workspace yet ---

/// The invite `good` joins; any other is refused with nothing made.
#[derive(Default)]
struct FakeJoining {
    calls: Mutex<Vec<String>>,
}

impl rodu_http::Joining for FakeJoining {
    fn join(
        &self,
        request: &rodu_api::JoinRequest,
    ) -> (rodu_core::Result<rodu_api::TeamDone>, bool) {
        let who = request.name.as_deref().or(request.as_name.as_deref()).unwrap_or("-");
        self.calls.lock().unwrap().push(format!("join {} {who}", request.code));
        match request.code.as_str() {
            "good" => (Ok(rodu_api::TeamDone { message: "Joined the team".into() }), true),
            _ => (Err(rodu_core::RoduError::invalid("not an invite")), false),
        }
    }
}

async fn joining_server(joining: std::sync::Arc<FakeJoining>) -> RunningServer {
    start_web_server(WebServerOptions {
        service: RoduService::new(SqliteStore::memory().unwrap()),
        actor: Actor { principal_id: String::new(), via_agent_id: None },
        port: 0,
        dist_dir: None,
        files: None,
        token: Some(TOKEN.into()),
        live: None,
        team: None,
        sharing: None,
        joining: Some(joining),
    })
    .await
    .unwrap()
}

fn join_body(body: Value) -> Opts<'static> {
    Opts { body: Some(body), ..Opts::default() }
}

#[tokio::test]
async fn with_no_workspace_only_the_join_is_served() {
    let joining = std::sync::Arc::new(FakeJoining::default());
    let s = joining_server(joining.clone()).await;
    for (method, path) in [
        ("GET", "/api/me"),
        ("GET", "/api/collections"),
        ("GET", "/api/board?collection=X"),
        ("GET", "/api/revision"),
        ("GET", "/api/team"),
        ("GET", "/api/team/places"),
    ] {
        let reply = send(&s, method, path, none()).await;
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
        assert_eq!(reply.json()["code"], "no_workspace", "{path}");
    }
    let made = Opts { body: Some(json!({"title": "nowhere"})), ..Opts::default() };
    assert_eq!(send(&s, "POST", "/api/items", made).await.json()["code"], "no_workspace");
    assert!(joining.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_join_that_went_through_asks_to_reopen_and_a_refused_one_does_not() {
    let joining = std::sync::Arc::new(FakeJoining::default());
    let s = joining_server(joining.clone()).await;
    let reply = send(&s, "POST", "/api/team/join", join_body(json!({"code": "bad"}))).await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(!reopen_asked(&s).await);
    assert_eq!(send(&s, "GET", "/api/me", none()).await.json()["code"], "no_workspace");

    let body = json!({"code": "good", "name": "bob"});
    let reply = send(&s, "POST", "/api/team/join", join_body(body)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["message"], "Joined the team");
    assert!(reopen_asked(&s).await);
    // The page waits for the board by this: the stand-in is not answered from any more.
    let reply = send(&s, "GET", "/api/me", none()).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["code"], "reopening");
    // Joined once: a second join is not run on top of it.
    let again = json!({"code": "good", "as": "bob"});
    let reply = send(&s, "POST", "/api/team/join", join_body(again)).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["code"], "reopening");
    assert_eq!(*joining.calls.lock().unwrap(), ["join bad -", "join good bob"]);
}

#[tokio::test]
async fn the_join_route_needs_the_token_the_right_method_and_a_known_body() {
    let joining = std::sync::Arc::new(FakeJoining::default());
    let s = joining_server(joining.clone()).await;
    let no_token =
        Opts { headers: vec![("Authorization", "")], ..join_body(json!({"code": "good"})) };
    assert_eq!(send(&s, "POST", "/api/team/join", no_token).await.status, 401);
    assert_eq!(send(&s, "GET", "/api/team/join", none()).await.status, 405);
    for body in
        [json!({"code": "good", "folder": "/tmp"}), json!({"name": "bob"}), json!({"code": 7})]
    {
        assert_eq!(send(&s, "POST", "/api/team/join", join_body(body)).await.status, 400);
    }
    assert!(joining.calls.lock().unwrap().is_empty(), "nothing reached the join");
    assert!(!reopen_asked(&s).await);
}

#[tokio::test]
async fn a_board_that_is_there_is_not_joined_over() {
    let s = sharing_server(None, None, std::sync::Arc::default()).await;
    let reply = send(&s, "POST", "/api/team/join", join_body(json!({"code": "good"}))).await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert!(!reopen_asked(&s).await);
}

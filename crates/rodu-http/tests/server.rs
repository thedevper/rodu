use std::collections::HashMap;

use axum::body::Bytes;
use rodu_core::{Actor, PrincipalKind, RoduService};
use rodu_http::{RunningServer, WebServerOptions, start_web_server};
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

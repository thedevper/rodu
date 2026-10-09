//! The local API behind the web board, and the board's static files.
//!
//! It binds 127.0.0.1 only, accepts only its own Host names (against DNS rebinding) and needs a
//! Bearer token on every `/api/` call. Errors share one JSON shape: `{code, message, hint}`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
pub use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use rodu_api::{
    BoardView, CollectionView, CommentRequest, CommentView, CreateRequest, ErrorBody, ItemDetail,
    ItemView, MeView, MoveRequest, PatchRequest, PrincipalView, RevisionView, StateView,
    TransitionRequest,
};
use rodu_core::query::parse_query;
use rodu_core::service::Placement;
use rodu_core::store::{Store, TxMode};
use rodu_core::{
    Actor, Collection, Comment, ErrorCode, Item, Live, LiveSync, RoduError, RoduService,
};
use rodu_store::compile::{CompileContext, to_sql};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const BOARD_PAGE: u32 = 100;
const BOARD_MAX_ITEMS: usize = 1000;
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        // 'wasm-unsafe-eval' lets the page compile its own WebAssembly; it does not allow eval().
        "default-src 'self'; img-src 'self' data:; style-src 'self'; \
         script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'none'",
    ),
    ("referrer-policy", "no-referrer"),
    ("x-content-type-options", "nosniff"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
];

pub struct WebServerOptions<S: Store> {
    pub service: RoduService<S>,
    pub actor: Actor,
    /// Live sync while the server runs, such as with a team folder.
    pub live: Option<LiveOptions<S>>,
    /// 0 picks a free port.
    pub port: u16,
    /// Built web UI on disk to serve at /.
    pub dist_dir: Option<PathBuf>,
    /// The built UI held in memory ("/index.html" → bytes), as the single binary carries it.
    /// Takes precedence over `dist_dir`. With neither, only the API is served.
    pub files: Option<HashMap<String, Bytes>>,
    /// Fixed token for tests; a random one is generated otherwise.
    pub token: Option<String>,
}

pub struct RunningServer {
    pub url: String,
    pub port: u16,
    pub token: String,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    /// Stops accepting connections and lets in-flight requests finish, for up to five seconds;
    /// a client that stalls longer is cut off so Ctrl+C always exits.
    pub async fn close(self) -> std::io::Result<()> {
        let _ = self.shutdown.send(());
        let abort = self.task.abort_handle();
        match tokio::time::timeout(SHUTDOWN_GRACE, self.task).await {
            Ok(joined) => joined.map_err(std::io::Error::other)?,
            Err(_) => {
                abort.abort();
                Ok(())
            }
        }
    }
}

/// How a server syncs while it runs: a pull and a push every `every`, and a push after each write.
pub struct LiveOptions<S: Store> {
    pub sync: Arc<dyn LiveSync<S>>,
    pub every: std::time::Duration,
}

/// The interval `rodu web` syncs at.
pub const LIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// A failure answered with the shared error JSON.
enum Failure {
    Http { status: StatusCode, code: &'static str, message: String },
    Domain(RoduError),
}

impl From<RoduError> for Failure {
    fn from(error: RoduError) -> Self {
        Failure::Domain(error)
    }
}

fn http(status: StatusCode, code: &'static str, message: impl Into<String>) -> Failure {
    Failure::Http { status, code, message: message.into() }
}

type Outcome<T> = std::result::Result<T, Failure>;

struct App<S: Store> {
    service: Mutex<RoduService<S>>,
    actor: Actor,
    token_digest: [u8; 32],
    allowed_hosts: [String; 2],
    dist_dir: Option<PathBuf>,
    files: Option<HashMap<String, Bytes>>,
    live: Option<Live<S>>,
    /// Raised by every pull that took something in and every write, so the board can ask
    /// cheaply whether to reload.
    revision: AtomicU64,
}

pub async fn start_web_server<S>(options: WebServerOptions<S>) -> std::io::Result<RunningServer>
where
    S: Store + Send + 'static,
{
    let token = match options.token {
        Some(token) => token,
        None => random_token()?,
    };
    let dist_dir = match options.dist_dir {
        Some(dir) => Some(std::fs::canonicalize(dir)?),
        None => None,
    };
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], options.port))).await?;
    let port = listener.local_addr()?.port();
    let app = Arc::new(App {
        service: Mutex::new(options.service),
        actor: options.actor,
        token_digest: digest(&token),
        allowed_hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        dist_dir,
        files: options.files,
        live: options.live.as_ref().map(|live| Live::new(Arc::clone(&live.sync))),
        revision: AtomicU64::new(0),
    });
    // The live sync timer runs in the server's own task, so it stops with the server.
    let ticking = options.live.map(|live| (Arc::clone(&app), live.every));
    let router = Router::new().fallback(handle::<S>).with_state(app);
    let (shutdown, signal) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let serve = axum::serve(listener, router).with_graceful_shutdown(async {
            let _ = signal.await;
        });
        match ticking {
            None => serve.await,
            Some((app, every)) => tokio::select! {
                served = serve => served,
                () = tick(app, every) => Ok(()),
            },
        }
    });
    Ok(RunningServer { url: format!("http://127.0.0.1:{port}/"), port, token, shutdown, task })
}

/// Pulls and pushes every `every` while the server runs, under the service lock like a request.
async fn tick<S: Store + Send + 'static>(app: Arc<App<S>>, every: std::time::Duration) {
    let mut timer = tokio::time::interval(every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        let app = Arc::clone(&app);
        let _ = tokio::task::spawn_blocking(move || {
            let (Some(live), Ok(service)) = (&app.live, app.service.lock()) else { return };
            if live.pull(&service) {
                app.revision.fetch_add(1, Ordering::SeqCst);
            }
            live.push(&service);
        })
        .await;
    }
}

fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(hex::encode(bytes))
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// Compares digests without an early exit, so timing says nothing about the token.
fn same_digest(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn handle<S>(State(app): State<Arc<App<S>>>, request: Request) -> Response
where
    S: Store + Send + 'static,
{
    match route(app, request).await {
        Ok(response) => response,
        Err(failure) => error_response(failure),
    }
}

async fn route<S>(app: Arc<App<S>>, request: Request) -> Outcome<Response>
where
    S: Store + Send + 'static,
{
    // Only exact local hosts: a DNS-rebinding page arrives with its own host name.
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()))
        .unwrap_or_default();
    if !app.allowed_hosts.contains(&host) {
        return Err(http(StatusCode::FORBIDDEN, "forbidden", "Unexpected Host header"));
    }
    let path = request.uri().path().to_owned();
    if path.starts_with("/api/") {
        if !authorized(&app.token_digest, request.headers()) {
            return Err(http(StatusCode::UNAUTHORIZED, "unauthorized", "Missing or wrong token"));
        }
        let (status, data) = api(app, request).await?;
        return Ok(json_response(status, &data));
    }
    let method = request.method();
    if method != Method::GET && method != Method::HEAD {
        return Err(http(StatusCode::METHOD_NOT_ALLOWED, "invalid", "Method not allowed"));
    }
    serve_static(&app, &path).await
}

fn authorized(expected: &[u8; 32], headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else { return false };
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return false;
    }
    same_digest(&digest(token), expected)
}

/// The API routes. The route and method are checked before any body is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Me,
    Collections,
    Principals,
    Board,
    Create,
    Item,
    Transition,
    Move,
    Comments,
    Revision,
}

fn resolve(path: &str) -> Option<(Endpoint, Option<&str>, &'static [Method])> {
    const GET: &[Method] = &[Method::GET];
    const POST: &[Method] = &[Method::POST];
    const GET_PATCH: &[Method] = &[Method::GET, Method::PATCH];
    match path {
        "/api/me" => return Some((Endpoint::Me, None, GET)),
        "/api/collections" => return Some((Endpoint::Collections, None, GET)),
        "/api/principals" => return Some((Endpoint::Principals, None, GET)),
        "/api/board" => return Some((Endpoint::Board, None, GET)),
        "/api/revision" => return Some((Endpoint::Revision, None, GET)),
        "/api/items" => return Some((Endpoint::Create, None, POST)),
        _ => {}
    }
    let rest = path.strip_prefix("/api/items/")?;
    let (key, action) = match rest.split_once('/') {
        Some((key, action)) => (key, Some(action)),
        None => (rest, None),
    };
    if key.is_empty() {
        return None;
    }
    let endpoint = match action {
        None => return Some((Endpoint::Item, Some(key), GET_PATCH)),
        Some("transition") => Endpoint::Transition,
        Some("move") => Endpoint::Move,
        Some("comments") => Endpoint::Comments,
        Some(_) => return None,
    };
    Some((endpoint, Some(key), POST))
}

async fn api<S>(app: Arc<App<S>>, request: Request) -> Outcome<(StatusCode, serde_json::Value)>
where
    S: Store + Send + 'static,
{
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let Some((endpoint, raw_key, allowed)) = resolve(&path) else {
        return Err(http(StatusCode::NOT_FOUND, "not_found", format!("No route {method} {path}")));
    };
    if !allowed.contains(&method) {
        let names: Vec<&str> = allowed.iter().map(Method::as_str).collect();
        return Err(http(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid",
            format!("{method} is not allowed here; use {}", names.join(" or ")),
        ));
    }
    let key = match raw_key {
        Some(raw) => Some(
            decode_component(raw)
                .ok_or_else(|| http(StatusCode::BAD_REQUEST, "invalid", "Bad item key"))?,
        ),
        None => None,
    };
    let query: Vec<(String, String)> = request
        .uri()
        .query()
        .map(|q| serde_urlencoded::from_str(q).unwrap_or_default())
        .unwrap_or_default();
    let body = if method == Method::POST || method == Method::PATCH {
        Some(read_json(request).await?)
    } else {
        None
    };

    // SQLite work is blocking: run it off the async workers, one request at a time.
    tokio::task::spawn_blocking(move || {
        let service = app
            .service
            .lock()
            .map_err(|_| Failure::Domain(RoduError::internal("service lock poisoned")))?;
        let ctx = Ctx { service: &service, actor: &app.actor };
        let key = key.as_deref().unwrap_or_default();
        let body = body.unwrap_or(serde_json::Value::Null);
        let writes = method != Method::GET;
        let answer = match endpoint {
            Endpoint::Me => {
                ok(StatusCode::OK, &MeView { name: ctx.name(Some(&app.actor.principal_id))? })
            }
            Endpoint::Collections => {
                let all = service.list_collections()?;
                ok(StatusCode::OK, &all.iter().map(collection_view).collect::<Vec<_>>())
            }
            Endpoint::Principals => {
                let people: Vec<PrincipalView> = service
                    .store
                    .list_principals()?
                    .into_iter()
                    .map(|p| PrincipalView { name: p.name, kind: p.kind.as_str().to_owned() })
                    .collect();
                ok(StatusCode::OK, &people)
            }
            Endpoint::Board => {
                let param =
                    |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
                let collection = param("collection")
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| RoduError::invalid("collection is required"))?;
                ok(StatusCode::OK, &ctx.board(collection, param("q").unwrap_or(""))?)
            }
            Endpoint::Create => ctx.create(parse_body(body)?),
            Endpoint::Item if method == Method::GET => {
                let found = service.item(key)?;
                let comments = service
                    .store
                    .list_comments(&found.id)?
                    .iter()
                    .map(|c| ctx.comment_view(c))
                    .collect::<rodu_core::Result<Vec<_>>>()?;
                ok(StatusCode::OK, &ItemDetail { item: ctx.item_view(&found)?, comments })
            }
            Endpoint::Item => {
                let input: PatchRequest = parse_body(body)?;
                if input.expected_version.is_some_and(|v| v < 1) {
                    return Err(invalid_request("expectedVersion: must be at least 1"));
                }
                let updated =
                    service.update_item(&app.actor, key, &input.patch, input.expected_version)?;
                ok(StatusCode::OK, &ctx.item_view(&updated)?)
            }
            Endpoint::Transition => ctx.transition(key, parse_body(body)?),
            Endpoint::Move => {
                let input: MoveRequest = parse_body(body)?;
                check_ref("after", input.after.as_deref())?;
                check_ref("before", input.before.as_deref())?;
                let placement = Placement { after: input.after, before: input.before };
                let moved = service.move_item(&app.actor, key, &placement)?;
                ok(StatusCode::OK, &ctx.item_view(&moved)?)
            }
            Endpoint::Comments => {
                let input: CommentRequest = parse_body(body)?;
                check_len("body", &input.body, 20_000)?;
                let made = service.comment(&app.actor, key, &input.body)?;
                ok(StatusCode::CREATED, &ctx.comment_view(&made)?)
            }
            Endpoint::Revision => {
                ok(StatusCode::OK, &RevisionView { revision: app.revision.load(Ordering::SeqCst) })
            }
        };
        // A change made here goes out at once, and other tabs see it on their next look.
        if writes && answer.is_ok() {
            app.revision.fetch_add(1, Ordering::SeqCst);
            if let Some(live) = &app.live {
                live.push(&service);
            }
        }
        answer
    })
    .await
    .map_err(|e| Failure::Domain(RoduError::internal(format!("request task failed: {e}"))))?
}

struct Ctx<'a, S: Store> {
    service: &'a RoduService<S>,
    actor: &'a Actor,
}

impl<S: Store> Ctx<'_, S> {
    fn name(&self, id: Option<&str>) -> rodu_core::Result<Option<String>> {
        self.service.principal_name(id)
    }

    fn item_view(&self, item: &Item) -> rodu_core::Result<ItemView> {
        Ok(ItemView {
            key: item.key.clone(),
            title: item.title.clone(),
            body: item.body.clone(),
            item_type: item.item_type.as_str().to_owned(),
            status: item.status.clone(),
            category: item.category.as_str().to_owned(),
            priority: item.priority.as_str().to_owned(),
            assignee: self.name(item.assignee_id.as_deref())?,
            estimate: item.estimate,
            due: item.due_at.clone(),
            rank: item.rank.clone(),
            version: item.version,
        })
    }

    fn comment_view(&self, comment: &Comment) -> rodu_core::Result<CommentView> {
        Ok(CommentView {
            id: comment.id.clone(),
            author: self.name(Some(&comment.author_id))?.unwrap_or_else(|| "unknown".into()),
            via: self.name(comment.via_agent_id.as_deref())?,
            body: comment.body.clone(),
            created_at: comment.created_at.clone(),
        })
    }

    fn board(&self, collection_ref: &str, filter: &str) -> rodu_core::Result<BoardView> {
        let collection = self.service.collection(collection_ref)?;
        let mut query = format!("collection = \"{}\"", collection.key);
        if !filter.trim().is_empty() {
            // The filter must stand alone (balanced, no ORDER BY) so it cannot escape the
            // collection scope. Compile it alone first so errors point into the user's text.
            let ctx =
                CompileContext { me: Some(&self.actor.principal_id), now: self.service.now() };
            to_sql(filter, &ctx)?;
            if !parse_query(filter)?.order_by.is_empty() {
                return Err(RoduError::invalid("The board orders cards itself")
                    .with_hint("Remove ORDER BY"));
            }
            query.push_str(&format!(" AND ({filter})"));
        }
        query.push_str(" ORDER BY rank");
        // One read snapshot so concurrent writers cannot shift rows between pages.
        let (items, total) = self.service.store.transaction(TxMode::Read, || {
            let mut items: Vec<Item> = Vec::new();
            let mut total = 0;
            while items.len() < BOARD_MAX_ITEMS {
                let offset = u32::try_from(items.len()).unwrap_or(u32::MAX);
                let page =
                    self.service.search(self.actor, &query, Some(BOARD_PAGE), Some(offset))?;
                total = page.total;
                let full = page.items.len() == BOARD_PAGE as usize;
                items.extend(page.items);
                if !full {
                    break;
                }
            }
            Ok((items, total))
        })?;
        Ok(BoardView {
            collection: collection_view(&collection),
            items: items.iter().map(|i| self.item_view(i)).collect::<rodu_core::Result<_>>()?,
            total,
        })
    }

    fn create(&self, input: CreateRequest) -> Outcome<(StatusCode, serde_json::Value)> {
        check_len("collection", &input.collection, 20)?;
        if let Some(status) = &input.status {
            check_len("status", status, 40)?;
        }
        let service = self.service;
        // Created straight into a column: if the workflow refuses that status, nothing is created.
        let created = service.store.transaction(TxMode::Write, || {
            let made = service
                .create_items(
                    self.actor,
                    &input.collection,
                    std::slice::from_ref(&input.item),
                    None,
                )?
                .into_iter()
                .next()
                .ok_or_else(|| RoduError::internal("create returned nothing"))?;
            match &input.status {
                Some(status) if !made.status.eq_ignore_ascii_case(status) => {
                    service.transition(self.actor, &made.key, status)
                }
                _ => Ok(made),
            }
        })?;
        ok(StatusCode::CREATED, &self.item_view(&created)?)
    }

    fn transition(
        &self,
        key: &str,
        input: TransitionRequest,
    ) -> Outcome<(StatusCode, serde_json::Value)> {
        check_len("to", &input.to, 40)?;
        check_ref("after", input.after.as_deref())?;
        check_ref("before", input.before.as_deref())?;
        let service = self.service;
        // A drop into another column changes status and position together, or neither.
        let moved = service.store.transaction(TxMode::Write, || {
            let next = service.transition(self.actor, key, &input.to)?;
            if input.after.is_none() && input.before.is_none() {
                return Ok(next);
            }
            let placement = Placement { after: input.after.clone(), before: input.before.clone() };
            service.move_item(self.actor, &next.key, &placement)
        })?;
        ok(StatusCode::OK, &self.item_view(&moved)?)
    }
}

fn collection_view(c: &Collection) -> CollectionView {
    CollectionView {
        key: c.key.clone(),
        name: c.name.clone(),
        states: c
            .workflow
            .states
            .iter()
            .map(|s| StateView { name: s.name.clone(), category: s.category.as_str().to_owned() })
            .collect(),
    }
}

fn ok<T: Serialize>(status: StatusCode, data: &T) -> Outcome<(StatusCode, serde_json::Value)> {
    let value = serde_json::to_value(data)
        .map_err(|e| RoduError::internal(format!("serialize response: {e}")))?;
    Ok((status, value))
}

fn invalid_request(detail: impl std::fmt::Display) -> Failure {
    Failure::Domain(RoduError::invalid(format!("Invalid request: {detail}")))
}

fn parse_body<T: DeserializeOwned>(body: serde_json::Value) -> Outcome<T> {
    serde_json::from_value(body).map_err(invalid_request)
}

fn check_len(field: &str, value: &str, max: usize) -> Outcome<()> {
    let n = value.chars().count();
    if n == 0 {
        return Err(invalid_request(format!("{field}: must not be empty")));
    }
    if n > max {
        return Err(invalid_request(format!("{field}: at most {max} characters")));
    }
    Ok(())
}

fn check_ref(field: &str, value: Option<&str>) -> Outcome<()> {
    value.map_or(Ok(()), |v| check_len(field, v, 100))
}

async fn read_json(request: Request) -> Outcome<serde_json::Value> {
    let headers = request.headers();
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    if !content_type.is_some_and(is_json_type) {
        return Err(http(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid",
            "Send JSON with Content-Type: application/json",
        ));
    }
    let too_large =
        || http(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Request body is too large");
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if declared > MAX_BODY_BYTES as u64 {
        return Err(too_large());
    }
    let bytes =
        axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await.map_err(|_| too_large())?;
    serde_json::from_slice(&bytes)
        .map_err(|_| http(StatusCode::BAD_REQUEST, "invalid", "Body is not valid JSON"))
}

fn is_json_type(value: &str) -> bool {
    const JSON: &str = "application/json";
    value.len() >= JSON.len()
        && value.is_char_boundary(JSON.len())
        && value[..JSON.len()].eq_ignore_ascii_case(JSON)
        && matches!(value.as_bytes().get(JSON.len()), None | Some(b';'))
}

/// Strict percent-decoding (like `decodeURIComponent`): a broken escape or invalid UTF-8 is None.
fn decode_component(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The URL path as clean segments: decoded, with `.` and `..` resolved inside the root (as a
/// browser does), so the result can never point outside the served directory.
fn clean_segments(path: &str) -> Outcome<Vec<String>> {
    let decoded = decode_component(path)
        .ok_or_else(|| http(StatusCode::BAD_REQUEST, "invalid", "Bad path"))?;
    let mut segments: Vec<String> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            // Separators and drive prefixes a Windows path would honour.
            s if s.contains(['\\', ':', '\0']) => {
                return Err(http(StatusCode::NOT_FOUND, "not_found", "Not found"));
            }
            s => segments.push(s.to_owned()),
        }
    }
    Ok(segments)
}

fn has_extension(segments: &[String]) -> bool {
    segments.last().and_then(|s| s.rfind('.')).is_some_and(|dot| dot > 0)
}

async fn serve_static<S: Store>(app: &App<S>, path: &str) -> Outcome<Response> {
    if app.dist_dir.is_none() && app.files.is_none() {
        return Err(http(StatusCode::NOT_FOUND, "not_found", "The web UI is not built"));
    }
    let segments = clean_segments(path)?;
    let not_found = || http(StatusCode::NOT_FOUND, "not_found", "Not found");
    let (name, content) = if let Some(files) = &app.files {
        let wanted = format!("/{}", segments.join("/"));
        match files.get(&wanted) {
            Some(content) if !segments.is_empty() => (wanted, content.clone()),
            // Unknown paths without an extension are app routes: serve the single page.
            _ if has_extension(&segments) => return Err(not_found()),
            _ => {
                let index = files.get("/index.html").ok_or_else(not_found)?;
                ("/index.html".to_owned(), index.clone())
            }
        }
    } else {
        let root = app.dist_dir.as_ref().ok_or_else(not_found)?;
        let mut file = root.clone();
        file.extend(&segments);
        // Resolve symlinks and stay inside the root, whatever the folder links to.
        let inside = tokio::fs::canonicalize(&file).await.ok().filter(|f| f.starts_with(root));
        let is_file = match &inside {
            Some(f) => tokio::fs::metadata(f).await.is_ok_and(|m| m.is_file()),
            None => false,
        };
        if tokio::fs::symlink_metadata(&file).await.is_ok() && inside.is_none() {
            return Err(not_found());
        }
        if !is_file {
            if has_extension(&segments) {
                return Err(not_found());
            }
            file = root.join("index.html");
        }
        let content = tokio::fs::read(&file).await.map_err(|_| not_found())?;
        (file.to_string_lossy().into_owned(), Bytes::from(content))
    };
    // Built files keep fixed names (rodu_web_bg.wasm), so they must be revalidated, not pinned.
    let cache = if name.ends_with("index.html") { "no-store" } else { "no-cache" };
    let mut response = Response::new(Body::from(content));
    let headers = response.headers_mut();
    add_security_headers(headers);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(&name)));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    Ok(response)
}

fn mime(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn add_security_headers(headers: &mut HeaderMap) {
    for (name, value) in SECURITY_HEADERS {
        headers.insert(*name, HeaderValue::from_static(value));
    }
}

fn json_response<T: Serialize>(status: StatusCode, data: &T) -> Response {
    let body = serde_json::to_vec(data).unwrap_or_else(|_| b"null".to_vec());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    add_security_headers(headers);
    headers
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error_response(failure: Failure) -> Response {
    let (status, body) = match failure {
        Failure::Http { status, code, message } => {
            (status, ErrorBody { code: code.to_owned(), message, hint: None })
        }
        Failure::Domain(error) if error.code == ErrorCode::Internal => {
            // Unexpected failures stay generic so paths and SQL never reach the page.
            eprintln!("rodu web: internal error: {}", error.message);
            let body = ErrorBody {
                code: "internal".to_owned(),
                message: "Internal error".to_owned(),
                hint: None,
            };
            (StatusCode::INTERNAL_SERVER_ERROR, body)
        }
        Failure::Domain(error) => {
            let status = match error.code {
                ErrorCode::NotFound => StatusCode::NOT_FOUND,
                ErrorCode::Conflict => StatusCode::CONFLICT,
                ErrorCode::RuleViolation => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::BAD_REQUEST,
            };
            let body = ErrorBody {
                code: error.code.as_str().to_owned(),
                message: error.message,
                hint: error.hint,
            };
            (status, body)
        }
    };
    json_response(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_strictly() {
        assert_eq!(decode_component("DEMO-1").as_deref(), Some("DEMO-1"));
        assert_eq!(decode_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_component("%E0%A4%A"), None);
        assert_eq!(decode_component("%zz"), None);
        assert_eq!(decode_component("%FF"), None);
    }

    #[test]
    fn keeps_paths_inside_the_root() {
        assert_eq!(clean_segments("/../package.json").ok(), Some(vec!["package.json".into()]));
        assert_eq!(
            clean_segments("/%2e%2e/%2e%2e/etc/passwd").ok(),
            Some(vec!["etc".to_owned(), "passwd".to_owned()])
        );
        assert!(clean_segments("/..%5c..%5cwin.ini").is_err());
        assert!(clean_segments("/C:%2fx").is_err());
    }

    #[test]
    fn matches_json_content_types() {
        assert!(is_json_type("application/json"));
        assert!(is_json_type("Application/JSON; charset=utf-8"));
        assert!(!is_json_type("application/jsonx"));
        assert!(!is_json_type("text/plain"));
    }

    #[test]
    fn routes_before_methods() {
        assert!(resolve("/api/nope").is_none());
        assert!(resolve("/api/items/").is_none());
        assert!(resolve("/api/items/X/other").is_none());
        assert!(resolve("/api/items/X/move").is_some_and(|r| r.0 == Endpoint::Move));
        assert!(resolve("/api/items/X").is_some_and(|r| r.2.contains(&Method::PATCH)));
    }
}

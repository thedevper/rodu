//! Client for the local API served by `rodu web`. The JSON contract lives in `rodu-api`.

use gloo_net::http::{Method, RequestBuilder};
use rodu_api::{
    BoardView, CollectionView, CommentRequest, CommentView, CreateRequest, ErrorBody, ItemDetail,
    ItemView, MeView, MoveRequest, PatchRequest, PrincipalView, RevisionView, TransitionRequest,
};
use serde::{Serialize, de::DeserializeOwned};

const TOKEN_KEY: &str = "rodu.token";

/// A failed request, ready to show: the server's message and hint, or what went wrong locally.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    /// HTTP status; 0 when no response arrived.
    pub status: u16,
    pub message: String,
    pub hint: Option<String>,
}

impl ApiError {
    fn local(message: impl Into<String>) -> Self {
        Self { status: 0, message: message.into(), hint: None }
    }
}

/// `rodu web` opens the page with `#token=...`. The fragment never reaches the server; move it
/// into sessionStorage and strip it from the address bar so it is not bookmarked or shared.
pub fn take_token() -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.session_storage().ok().flatten();
    let location = window.location();
    if let Some(token) = location.hash().ok().and_then(|hash| crate::board::token_from_hash(&hash))
    {
        // Storage can be unavailable; then the token lives in memory for this page only.
        if let Some(storage) = &storage {
            let _ = storage.set_item(TOKEN_KEY, &token);
        }
        let path = location.pathname().unwrap_or_default() + &location.search().unwrap_or_default();
        if let Ok(history) = window.history() {
            let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&path));
        }
        return Some(token);
    }
    storage.and_then(|s| s.get_item(TOKEN_KEY).ok().flatten())
}

fn encode(component: &str) -> String {
    js_sys::encode_uri_component(component).into()
}

fn item_path(key: &str) -> String {
    format!("/api/items/{}", encode(key))
}

/// The API with the page's bearer token. `Copy`, so event handlers can capture it freely.
#[derive(Debug, Clone, Copy)]
pub struct Api {
    token: &'static str,
}

impl Api {
    /// The token lives as long as the page, so leaking this one string is deliberate.
    pub fn new(token: String) -> Self {
        Self { token: Box::leak(token.into_boxed_str()) }
    }

    async fn call<T: DeserializeOwned>(
        self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, ApiError> {
        let builder = RequestBuilder::new(path)
            .method(method)
            .header("Authorization", &format!("Bearer {}", self.token));
        let request = match body {
            Some(body) => builder.json(body),
            None => builder.build(),
        }
        .map_err(|e| ApiError::local(e.to_string()))?;
        let response = request.send().await.map_err(|e| ApiError::local(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !response.ok() {
            return Err(match serde_json::from_str::<ErrorBody>(&text) {
                Ok(body) => ApiError { status, message: body.message, hint: body.hint },
                Err(_) => {
                    ApiError { status, message: format!("Request failed ({status})"), hint: None }
                }
            });
        }
        serde_json::from_str(&text).map_err(|e| ApiError {
            status,
            message: format!("Unexpected response: {e}"),
            hint: None,
        })
    }

    async fn get<T: DeserializeOwned>(self, path: &str) -> Result<T, ApiError> {
        self.call(Method::GET, path, None::<&()>).await
    }

    pub async fn me(self) -> Result<MeView, ApiError> {
        self.get("/api/me").await
    }

    pub async fn collections(self) -> Result<Vec<CollectionView>, ApiError> {
        self.get("/api/collections").await
    }

    pub async fn revision(self) -> Result<u64, ApiError> {
        Ok(self.get::<RevisionView>("/api/revision").await?.revision)
    }

    pub async fn principals(self) -> Result<Vec<PrincipalView>, ApiError> {
        self.get("/api/principals").await
    }

    pub async fn board(self, collection: &str, q: &str) -> Result<BoardView, ApiError> {
        self.get(&format!("/api/board?collection={}&q={}", encode(collection), encode(q))).await
    }

    /// One request: if the workflow refuses `status`, no item is created.
    pub async fn create(
        self,
        collection: &str,
        title: &str,
        status: &str,
    ) -> Result<ItemView, ApiError> {
        let body = CreateRequest {
            collection: collection.to_string(),
            item: serde_json::json!({ "title": title }),
            status: Some(status.to_string()),
        };
        self.call(Method::POST, "/api/items", Some(&body)).await
    }

    pub async fn item(self, key: &str) -> Result<ItemDetail, ApiError> {
        self.get(&item_path(key)).await
    }

    pub async fn update(
        self,
        key: &str,
        patch: serde_json::Value,
        expected_version: i64,
    ) -> Result<ItemView, ApiError> {
        let body = PatchRequest { patch, expected_version: Some(expected_version) };
        self.call(Method::PATCH, &item_path(key), Some(&body)).await
    }

    /// Status and position change together on the server, or not at all.
    pub async fn transition(
        self,
        key: &str,
        to: &str,
        position: Option<MoveRequest>,
    ) -> Result<ItemView, ApiError> {
        let position = position.unwrap_or_default();
        let body = TransitionRequest {
            to: to.to_string(),
            after: position.after,
            before: position.before,
        };
        self.call(Method::POST, &format!("{}/transition", item_path(key)), Some(&body)).await
    }

    pub async fn move_to(self, key: &str, to: MoveRequest) -> Result<ItemView, ApiError> {
        self.call(Method::POST, &format!("{}/move", item_path(key)), Some(&to)).await
    }

    pub async fn comment(self, key: &str, body: &str) -> Result<CommentView, ApiError> {
        let body = CommentRequest { body: body.to_string() };
        self.call(Method::POST, &format!("{}/comments", item_path(key)), Some(&body)).await
    }
}

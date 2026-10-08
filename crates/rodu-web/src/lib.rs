//! Rodu's web board: a Leptos client-side app compiled to WebAssembly and served by `rodu web`.
//!
//! [`board`] holds the pure logic and builds anywhere; the UI itself only exists on `wasm32`.

pub mod board;

#[cfg(target_arch = "wasm32")]
mod api;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod panel;

/// Mount the board into the page's `<body>`.
#[cfg(target_arch = "wasm32")]
pub fn start() {
    leptos::mount::mount_to_body(app::App);
}

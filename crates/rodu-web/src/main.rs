#[cfg(target_arch = "wasm32")]
fn main() {
    rodu_web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("rodu-web runs in the browser; build it with `cargo xtask web`.");
    std::process::exit(1);
}

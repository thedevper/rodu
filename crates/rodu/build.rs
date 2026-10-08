//! Embeds the built web board (`crates/rodu-web/dist`, made by `cargo xtask web`) so the single
//! `rodu` binary can serve it. Without a build, the binary still works; `rodu web` says how to fix it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = manifest.join("../rodu-web/dist");
    println!("cargo::rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
    }
    files.sort();
    let mut code = String::from("pub static WEB_FILES: &[(&str, &[u8])] = &[\n");
    for (url, path) in &files {
        writeln!(code, "    ({url:?}, include_bytes!({:?})),", path.display().to_string())
            .expect("write to string");
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("web_files.rs"), code).expect("write web_files.rs");
}

/// Every file under `dir` as ("/relative/url", absolute path), skipping dotfiles.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(dir).expect("read the web dist folder");
    for entry in entries {
        let path = entry.expect("read a dist entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let parts: Vec<String> =
                rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            let absolute = fs::canonicalize(&path).expect("canonical dist path");
            files.push((format!("/{}", parts.join("/")), absolute));
        }
    }
}
